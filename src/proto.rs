//! 极简 proto2 编解码 —— pbbp2.Frame（仅 9 字段，与 reference/feishu_ws_protocol.py 字节级一致）。
//!
//! message Header { required string key=1; required string value=2; }
//! message Frame {
//!   required uint64 SeqID=1; required uint64 LogID=2;
//!   required int32 service=3; required int32 method=4;
//!   repeated Header headers=5;
//!   optional string payload_encoding=6; optional string payload_type=7;
//!   optional bytes  payload=8; optional string LogIDNew=9;
//! }
//! 我们只读写 1,2,3,4,5,8 —— 与 Python encode_frame/decode_frame 完全对齐。

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Frame {
    pub seq_id: u64,                    // field 1 varint
    pub log_id: u64,                    // field 2 varint
    pub service: u32,                   // field 3 varint
    pub method: u32,                    // field 4 varint (0=CONTROL, 1=DATA)
    pub headers: Vec<(String, String)>, // field 5 repeated {1:key,2:value}
    pub payload: Vec<u8>,               // field 8 bytes
}

impl Frame {
    #[allow(dead_code)] // 调试/未来用（取某个 header 值）
    pub fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_var_field(&mut out, 1, self.seq_id);
        put_var_field(&mut out, 2, self.log_id);
        put_var_field(&mut out, 3, self.service as u64);
        put_var_field(&mut out, 4, self.method as u64);
        for (k, v) in &self.headers {
            let mut h = Vec::new();
            put_len_field(&mut h, 1, k.as_bytes());
            put_len_field(&mut h, 2, v.as_bytes());
            put_len_field(&mut out, 5, &h);
        }
        if !self.payload.is_empty() {
            put_len_field(&mut out, 8, &self.payload);
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Frame> {
        let mut f = Frame::default();
        let mut i = 0usize;
        let n = buf.len();
        while i < n {
            let key = get_varint(buf, &mut i)?;
            let field = key >> 3;
            let wt = key & 7;
            match wt {
                0 => {
                    let val = get_varint(buf, &mut i)?;
                    match field {
                        1 => f.seq_id = val,
                        2 => f.log_id = val,
                        3 => f.service = val as u32,
                        4 => f.method = val as u32,
                        _ => {}
                    }
                }
                2 => {
                    let len = get_varint(buf, &mut i)? as usize;
                    // 用减法判边界（len > n - i），不用 i + len —— 恶意长度前缀（接近 usize::MAX）
                    // 会让加法溢出绕过检查，后面切片越界 panic 拖垮整个事件循环。
                    if len > n - i {
                        return None;
                    }
                    let data = &buf[i..i + len];
                    i += len;
                    match field {
                        5 => {
                            // 嵌套 Header { key=1, value=2 }
                            let (mut hk, mut hv) = (None, None);
                            let mut j = 0usize;
                            while j < data.len() {
                                let k2 = get_varint(data, &mut j)?;
                                let f2 = k2 >> 3;
                                let wt2 = k2 & 7;
                                if wt2 == 2 {
                                    let l2 = get_varint(data, &mut j)? as usize;
                                    if l2 > data.len() - j {
                                        return None;
                                    }
                                    let v2 = &data[j..j + l2];
                                    j += l2;
                                    let s = String::from_utf8_lossy(v2).into_owned();
                                    if f2 == 1 {
                                        hk = Some(s);
                                    } else if f2 == 2 {
                                        hv = Some(s);
                                    }
                                } else {
                                    // 非长度型，跳过 varint
                                    get_varint(data, &mut j)?;
                                }
                            }
                            // 缺 key 或 value 的畸形 header：跳过该条即可，别用 ? 把整帧 decode 拖死
                            // （DATA 帧解析失败 = 静默丢事件且不回 ack，飞书还会重投 → 卡循环）。
                            if let (Some(hk), Some(hv)) = (hk, hv) {
                                f.headers.push((hk, hv));
                            }
                        }
                        8 => f.payload = data.to_vec(),
                        _ => {}
                    }
                }
                _ => break, // 其它 wire type：跳出（对齐 Python 的 break）
            }
        }
        Some(f)
    }
}

fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n != 0 {
            out.push(b | 0x80);
        } else {
            out.push(b);
            break;
        }
    }
}

fn get_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut shift = 0u32;
    let mut res = 0u64;
    loop {
        if *i >= b.len() {
            return None;
        }
        let byte = b[*i];
        *i += 1;
        res |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(res);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn put_tag(out: &mut Vec<u8>, field: u64, wt: u64) {
    put_varint(out, (field << 3) | wt);
}

fn put_var_field(out: &mut Vec<u8>, field: u64, val: u64) {
    put_tag(out, field, 0);
    put_varint(out, val);
}

fn put_len_field(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    put_tag(out, field, 2);
    put_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for n in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, n);
            let mut i = 0;
            assert_eq!(get_varint(&buf, &mut i), Some(n));
            assert_eq!(i, buf.len());
        }
    }

    #[test]
    fn frame_roundtrip() {
        let f = Frame {
            seq_id: 7,
            log_id: 9,
            service: 33554678,
            method: 1,
            headers: vec![
                ("type".into(), "event".into()),
                ("message_id".into(), "om_abc123".into()),
                ("sum".into(), "1".into()),
                ("seq".into(), "0".into()),
            ],
            payload: br#"{"schema":"2.0"}"#.to_vec(),
        };
        let enc = f.encode();
        let dec = Frame::decode(&enc).expect("decode");
        assert_eq!(f, dec);
        assert_eq!(dec.header("type"), Some("event"));
        assert_eq!(dec.header("message_id"), Some("om_abc123"));
    }

    #[test]
    fn control_ping_no_payload() {
        let f = Frame {
            seq_id: 0,
            log_id: 0,
            service: 33554678,
            method: 0,
            headers: vec![("type".into(), "ping".into())],
            payload: vec![],
        };
        let enc = f.encode();
        let dec = Frame::decode(&enc).expect("decode");
        assert_eq!(dec.method, 0);
        assert_eq!(dec.header("type"), Some("ping"));
        assert!(dec.payload.is_empty());
    }

    /// 与 reference/feishu_ws_protocol.py 的 encode_frame 对拍：同一组参数必须产生相同字节。
    /// Python: encode_frame(7, 9, 33554678, 1, [("type","event"),("message_id","om_x")], b'{"code":200}')
    #[test]
    fn matches_python_encode() {
        let f = Frame {
            seq_id: 7,
            log_id: 9,
            service: 33554678,
            method: 1,
            headers: vec![
                ("type".into(), "event".into()),
                ("message_id".into(), "om_x".into()),
            ],
            payload: br#"{"code":200}"#.to_vec(),
        };
        let enc = f.encode();
        // 由 Python 生成的期望字节（见 tests/gen_expected.py 输出），hex 形式
        let expected = hex_decode(PYTHON_EXPECTED_HEX);
        assert_eq!(enc, expected, "encode 必须与 Python encode_frame 字节一致");
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    const PYTHON_EXPECTED_HEX: &str = "0807100918f681801020012a0d0a047479706512056576656e742a120a0a6d6573736167655f696412046f6d5f78420c7b22636f6465223a3230307d";
}
