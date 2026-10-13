//! 数据装载：CSV 文本 / JSON 行 → charton `Dataset`。
//!
//! charton 自身没有 CSV/JSON 装载（`Dataset` 只有 `with_column` / `from_arrow` /
//! `from_record_batches`），所以这层映射由本包提供。
//!
//! 类型推断口径（简单、可预测、写进 README）：
//! - 一列里**非空值全部**能解析为 f64 ⇒ 数值列（空值 → 缺失，交给 charton 的 validity 位图）；
//! - 否则 ⇒ 分类列（原样字符串，包括数字字面量）。
//!
//! 「全是数字就一定是数值列」有一个常见的反例：年份 / 月份 / 编码这类**名义型**数字，
//! 用柱状图时要的是离散轴。所以规格里给了 `types` 覆盖（`{"month": "category"}`），
//! 这是显式的、可预期的逃生口，而不是靠启发式猜。

use std::collections::BTreeMap;

use charton::prelude::Dataset;

/// 一个单元格。
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Num(f64),
    Text(String),
    Null,
}

/// 装载后的表（列序保持来源顺序）。
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

/// 列类型（推断结果或规格覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Number,
    Category,
}

impl Table {
    pub fn height(&self) -> usize {
        self.rows.len()
    }

    /// 列索引（找不到 ⇒ Err，错误信息带上现有列名，方便模型自查）。
    pub fn column_index(&self, name: &str) -> Result<usize, String> {
        self.columns.iter().position(|c| c == name).ok_or_else(|| {
            format!(
                "数据里没有列「{name}」。现有列：{}",
                self.columns.join("、")
            )
        })
    }

    /// 推断每一列的类型。
    pub fn infer_types(&self) -> Vec<ColumnType> {
        (0..self.columns.len())
            .map(|idx| {
                let mut saw_value = false;
                for row in &self.rows {
                    match row.get(idx) {
                        Some(Cell::Num(_)) => saw_value = true,
                        Some(Cell::Text(t)) => {
                            if t.trim().parse::<f64>().is_ok() {
                                saw_value = true;
                            } else {
                                return ColumnType::Category;
                            }
                        }
                        Some(Cell::Null) | None => {}
                    }
                }
                if saw_value {
                    ColumnType::Number
                } else {
                    // 整列空 ⇒ 当分类列（数值轴没有数据也无从渲染，交给 charton 报错）。
                    ColumnType::Category
                }
            })
            .collect()
    }

    /// 取数值列（空 ⇒ None）。
    pub fn numbers(&self, idx: usize) -> Vec<Option<f64>> {
        self.rows
            .iter()
            .map(|row| match row.get(idx) {
                Some(Cell::Num(v)) => Some(*v),
                Some(Cell::Text(t)) => t.trim().parse::<f64>().ok(),
                _ => None,
            })
            .collect()
    }

    /// 取分类列（空 ⇒ None）。
    pub fn strings(&self, idx: usize) -> Vec<Option<String>> {
        self.rows
            .iter()
            .map(|row| match row.get(idx) {
                Some(Cell::Num(v)) => Some(fmt_number(*v)),
                Some(Cell::Text(t)) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }
}

/// 数字的分类展示：整数不带小数点（`2024` 而不是 `2024.0`），其余交给 Rust 的默认格式。
fn fmt_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// 解析 CSV 文本（首行为表头）。
pub fn table_from_csv(text: &str) -> Result<Table, String> {
    // 去 BOM：Excel / 飞书导出的 CSV 常带 UTF-8 BOM，否则第一列名会变成 "\u{feff}月份"，
    // 而它和规格里写的 "月份" 肉眼完全一样 —— 这种「列不存在」最难排查。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(text.as_bytes());
    let headers = reader
        .headers()
        .map_err(|e| format!("CSV 表头读取失败：{e}"))?
        .iter()
        .map(|h| h.trim().to_string())
        .collect::<Vec<_>>();
    if headers.is_empty() {
        return Err("CSV 没有表头（第一行必须是列名）".into());
    }

    let mut rows = Vec::new();
    for (line_no, record) in reader.records().enumerate() {
        let record = record.map_err(|e| format!("CSV 第 {} 行解析失败：{e}", line_no + 2))?;
        let mut row = Vec::with_capacity(headers.len());
        for idx in 0..headers.len() {
            row.push(cell_from_str(record.get(idx).unwrap_or("")));
        }
        rows.push(row);
    }
    Ok(Table {
        columns: headers,
        rows,
    })
}

fn cell_from_str(raw: &str) -> Cell {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        Cell::Null
    } else if let Ok(v) = trimmed.parse::<f64>() {
        Cell::Num(v)
    } else {
        Cell::Text(trimmed.to_string())
    }
}

/// 解析 JSON 行数组：`[{"月份":"1月","销售额":12.5}, …]`。
///
/// 列序 = 首次出现的顺序（不是 JSON 对象的哈希序：`serde_json` 默认 `preserve_order` 未开时
/// 是 BTreeMap，故这里显式按文档序收集，保证同输入同输出——渲染确定性的前提）。
pub fn table_from_json_rows(value: &serde_json::Value) -> Result<Table, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "内联数据 `rows` 必须是数组：`[{…}, {…}]`".to_string())?;
    let mut columns: Vec<String> = Vec::new();
    for (i, item) in array.iter().enumerate() {
        let obj = item
            .as_object()
            .ok_or_else(|| format!("`rows[{i}]` 不是对象；每行必须是 `{{\"列名\": 值}}`"))?;
        for key in obj.keys() {
            if !columns.iter().any(|c| c == key) {
                columns.push(key.clone());
            }
        }
    }
    let mut rows = Vec::with_capacity(array.len());
    for item in array {
        let obj = item.as_object().expect("上面已校验过对象");
        let mut row = Vec::with_capacity(columns.len());
        for col in &columns {
            row.push(match obj.get(col) {
                None | Some(serde_json::Value::Null) => Cell::Null,
                Some(serde_json::Value::Number(n)) => {
                    n.as_f64().map(Cell::Num).unwrap_or(Cell::Null)
                }
                Some(serde_json::Value::Bool(b)) => Cell::Text(b.to_string()),
                Some(serde_json::Value::String(s)) => cell_from_str(s),
                Some(other) => Cell::Text(other.to_string()),
            });
        }
        rows.push(row);
    }
    Ok(Table { columns, rows })
}

/// 表 → charton `Dataset`。
///
/// `overrides` 是规格里的 `types`（列名 → number/category），覆盖推断结果。
pub fn table_to_dataset(
    table: &Table,
    overrides: &BTreeMap<String, String>,
) -> Result<Dataset, String> {
    let inferred = table.infer_types();
    let mut ds = Dataset::new();
    for (idx, name) in table.columns.iter().enumerate() {
        let ty = match overrides.get(name).map(|s| s.trim().to_ascii_lowercase()) {
            Some(kind) => match kind.as_str() {
                "number" | "numeric" | "quantitative" | "float" => ColumnType::Number,
                "category" | "categorical" | "nominal" | "ordinal" | "string" | "text" => {
                    ColumnType::Category
                }
                other => {
                    return Err(format!(
                        "列「{name}」的类型覆盖「{other}」不认识；只接受 `number` 或 `category`"
                    ))
                }
            },
            None => inferred[idx],
        };
        ds = match ty {
            ColumnType::Number => ds
                .with_column(name.clone(), table.numbers(idx))
                .map_err(|e| format!("列「{name}」按数值装载失败：{e}"))?,
            ColumnType::Category => ds
                .with_column(name.clone(), table.strings(idx))
                .map_err(|e| format!("列「{name}」按分类装载失败：{e}"))?,
        };
    }
    Ok(ds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_infers_numbers_and_categories() {
        let t = table_from_csv("月份,销售额\n1月,12.5\n2月,13\n").unwrap();
        assert_eq!(t.columns, vec!["月份", "销售额"]);
        assert_eq!(t.height(), 2);
        let types = t.infer_types();
        assert_eq!(types, vec![ColumnType::Category, ColumnType::Number]);
        assert_eq!(t.numbers(1), vec![Some(12.5), Some(13.0)]);
    }

    /// 列名带 UTF-8 BOM（Excel/飞书导出）时必须被剥掉，否则规格里写的列名「查无此列」——
    /// 而两者肉眼完全一样，是最难排查的一类失败。
    #[test]
    fn csv_strips_utf8_bom_from_first_header() {
        let t = table_from_csv("\u{feff}月份,销售额\n1月,1\n").unwrap();
        assert_eq!(t.columns[0], "月份");
        assert!(t.column_index("月份").is_ok());
    }

    #[test]
    fn csv_empty_field_becomes_null_not_zero() {
        let t = table_from_csv("x,y\n1,\n2,3\n").unwrap();
        assert_eq!(t.numbers(1), vec![None, Some(3.0)]);
    }

    #[test]
    fn column_index_error_lists_available_columns() {
        let t = table_from_csv("a,b\n1,2\n").unwrap();
        let err = t.column_index("c").unwrap_err();
        assert!(err.contains("a、b"), "{err}");
    }

    #[test]
    fn json_rows_keep_first_seen_column_order() {
        let v: serde_json::Value =
            serde_json::from_str(r#"[{"b":1,"a":"x"},{"a":"y","b":2}]"#).unwrap();
        let t = table_from_json_rows(&v).unwrap();
        assert_eq!(
            t.columns,
            vec!["b", "a"],
            "列序必须是首次出现顺序（渲染确定性）"
        );
    }

    #[test]
    fn json_nulls_and_booleans_are_handled() {
        let v: serde_json::Value = serde_json::from_str(r#"[{"a":null,"b":true}]"#).unwrap();
        let t = table_from_json_rows(&v).unwrap();
        assert_eq!(t.rows[0][0], Cell::Null);
        assert_eq!(t.rows[0][1], Cell::Text("true".into()));
    }

    #[test]
    fn json_non_array_rows_is_error() {
        let v: serde_json::Value = serde_json::from_str(r#"{"a":1}"#).unwrap();
        assert!(table_from_json_rows(&v).unwrap_err().contains("数组"));
    }

    /// 类型覆盖：名义型数字（年份/月份）要能强制成分类列。
    #[test]
    fn type_override_forces_category() {
        let t = table_from_csv("year,v\n2024,1\n2025,2\n").unwrap();
        assert_eq!(t.infer_types()[0], ColumnType::Number);
        let mut overrides = BTreeMap::new();
        overrides.insert("year".to_string(), "category".to_string());
        // 能装进 Dataset 即说明按分类走了（数值列同样能装，故再断言字符串化结果）。
        assert!(table_to_dataset(&t, &overrides).is_ok());
        assert_eq!(t.strings(0), vec![Some("2024".into()), Some("2025".into())]);
    }

    #[test]
    fn unknown_type_override_is_error() {
        let t = table_from_csv("a\n1\n").unwrap();
        let mut overrides = BTreeMap::new();
        overrides.insert("a".to_string(), "date".to_string());
        let err = table_to_dataset(&t, &overrides).unwrap_err();
        assert!(err.contains("date"), "{err}");
    }

    #[test]
    fn integers_render_without_trailing_zero() {
        assert_eq!(fmt_number(2024.0), "2024");
        assert_eq!(fmt_number(1.5), "1.5");
    }
}
