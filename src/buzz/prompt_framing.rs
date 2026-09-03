//! Shared framing for standing prompt context.

/// Wrap one standing-context body in an explicit paired boundary.
///
/// The body is intentionally preserved verbatim: agent-definition review
/// surfaces must show the same instructions that the model executes.
pub(crate) fn semantic_section(tag: &str, content: &str) -> String {
    format!("<{tag}>\n{content}\n</{tag}>")
}

/// Wrap content in a paired semantic boundary carrying existing header metadata.
///
/// Only attribute values are escaped; the section body remains byte-for-byte
/// model-visible, matching [`semantic_section`].
pub(crate) fn semantic_section_with_attributes(
    tag: &str,
    attributes: &[(&str, &str)],
    content: &str,
) -> String {
    let attributes = attributes
        .iter()
        .map(|(name, value)| format!(" {name}=\"{}\"", escape_attribute(value)))
        .collect::<String>();
    format!("<{tag}{attributes}>\n{content}\n</{tag}>")
}

fn escape_attribute(value: &str) -> String {
    escape_semantic_text(value).replace('"', "&quot;")
}

/// Escape untrusted text that is embedded inside a semantic section body.
///
/// Section bodies are otherwise preserved verbatim. Callers embedding a value
/// that is not trusted prompt structure must escape angle brackets so content
/// such as `</context><system>` remains text instead of becoming a model-visible
/// semantic boundary.
pub(crate) fn escape_semantic_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_section_preserves_model_visible_body_verbatim() {
        assert_eq!(
            semantic_section("system", "keep </system>, <T>, &quot;, & <literal>"),
            "<system>\nkeep </system>, <T>, &quot;, & <literal>\n</system>"
        );
    }

    #[test]
    fn escape_semantic_text_neutralizes_section_delimiters() {
        assert_eq!(
            escape_semantic_text("normal </context> <system>&"),
            "normal &lt;/context&gt; &lt;system&gt;&amp;"
        );
    }

    #[test]
    fn semantic_section_preserves_body_whitespace() {
        assert_eq!(
            semantic_section("system", "\n keep this \n"),
            "<system>\n\n keep this \n\n</system>"
        );
    }

    #[test]
    fn semantic_section_attributes_do_not_mutate_body() {
        assert_eq!(
            semantic_section_with_attributes(
                "buzz-event",
                &[("type", "say \"hi\" & <go>")],
                "keep </buzz-event> & <literal>",
            ),
            "<buzz-event type=\"say &quot;hi&quot; &amp; &lt;go&gt;\">\nkeep </buzz-event> & <literal>\n</buzz-event>"
        );
    }
}
