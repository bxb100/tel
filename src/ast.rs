//! 模板引擎：把字符串里的 `{{ path }}` 解析成 AST，并针对 JSON 上下文求值。
//!
//! 只有 `{{ }}` 内的内容会被执行，括号外的文本原样保留。表达式目前只支持
//! 路径（`ctx.page.url`、`items[0]`）；运算符可在 [`Parser::expr`] 一层扩展。

use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::borrow::Cow;
use std::fmt;
use std::iter::Peekable;
use std::str::Chars;

// ───────────── AST ─────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Key(String),  // .name
    Index(usize), // [0]
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Path(Vec<Seg>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part<'a> {
    Text(&'a str),
    Expr(Expr),
}

pub type Template<'a> = Vec<Part<'a>>;

/// 把路径还原成源码写法：`ctx.page.url`、`items[0]`。
fn fmt_path(segs: &[Seg]) -> impl fmt::Display + '_ {
    struct Path<'a>(&'a [Seg]);

    impl fmt::Display for Path<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            for (i, seg) in self.0.iter().enumerate() {
                match seg {
                    Seg::Key(key) if i == 0 => write!(f, "{key}")?,
                    Seg::Key(key) => write!(f, ".{key}")?,
                    Seg::Index(index) => write!(f, "[{index}]")?,
                }
            }
            Ok(())
        }
    }

    Path(segs)
}

// ───────────── 解析：模板 → Vec<Part> ─────────────

pub fn parse_template(src: &str) -> Result<Template<'_>> {
    let mut parts = Vec::new();
    let mut rest = src;
    while let Some(start) = rest.find("{{") {
        if start > 0 {
            parts.push(Part::Text(&rest[..start]));
        }
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| anyhow!("unclosed `{{{{`"))?;
        let inner = &after[..end];
        let expr =
            parse_expr(inner).map_err(|err| anyhow!("{err} in `{{{{{}}}}}`", inner.trim()))?;
        parts.push(Part::Expr(expr));
        rest = &after[end + 2..];
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest));
    }
    Ok(parts)
}

// ───────────── 解析：`{{ }}` 内部 → Expr ─────────────

struct Parser<'a> {
    chars: Peekable<Chars<'a>>,
}

pub fn parse_expr(src: &str) -> Result<Expr> {
    let mut parser = Parser {
        chars: src.chars().peekable(),
    };
    parser.ws();
    let expr = parser.expr()?;
    parser.ws();
    match parser.chars.peek() {
        Some(c) => bail!("unexpected `{c}`"),
        None => Ok(expr),
    }
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.chars.next_if(|c| c.is_whitespace()).is_some() {}
    }

    /// 取出满足 `pred` 的连续字符。
    fn take_while(&mut self, pred: impl Fn(char) -> bool) -> String {
        let mut taken = String::new();
        while let Some(c) = self.chars.next_if(|c| pred(*c)) {
            taken.push(c);
        }
        taken
    }

    // 以后加运算符就在这一层做优先级处理: expr -> or -> and -> eq -> unary -> primary
    fn expr(&mut self) -> Result<Expr> {
        self.path()
    }

    fn ident(&mut self) -> Result<String> {
        let ident = self.take_while(|c| c.is_alphanumeric() || c == '_' || c == '-');
        if ident.is_empty() {
            bail!("expected identifier");
        }
        Ok(ident)
    }

    fn index(&mut self) -> Result<usize> {
        let digits = self.take_while(|c| c.is_ascii_digit());
        let index = digits
            .parse()
            .map_err(|_| anyhow!("expected array index, found `{digits}`"))?;
        if self.chars.next() != Some(']') {
            bail!("expected `]`");
        }
        Ok(index)
    }

    fn path(&mut self) -> Result<Expr> {
        let mut segs = vec![Seg::Key(self.ident()?)];
        loop {
            match self.chars.peek() {
                Some('.') => {
                    self.chars.next();
                    segs.push(Seg::Key(self.ident()?));
                }
                Some('[') => {
                    self.chars.next();
                    segs.push(Seg::Index(self.index()?));
                }
                _ => return Ok(Expr::Path(segs)),
            }
        }
    }
}

// ───────────── 求值：Expr + ctx → &Value ─────────────

/// 沿路径在 `ctx` 里取值，借用结果而不拷贝。
pub fn eval<'a>(expr: &Expr, ctx: &'a Value) -> Result<&'a Value> {
    let Expr::Path(segs) = expr;
    let mut cur = ctx;
    for (i, seg) in segs.iter().enumerate() {
        let next = match (seg, cur) {
            (Seg::Key(key), Value::Object(map)) => map.get(key),
            (Seg::Index(index), Value::Array(items)) => items.get(*index),
            _ => None,
        };
        cur = next.ok_or_else(|| {
            let parent = fmt_path(&segs[..i]);
            let reason = match (seg, cur) {
                (Seg::Key(key), Value::Object(_)) => format!("no key `{key}` in `{parent}`"),
                (Seg::Index(index), Value::Array(_)) => {
                    format!("index {index} out of range in `{parent}`")
                }
                (Seg::Key(_), _) => format!("`{parent}` is not an object"),
                (Seg::Index(_), _) => format!("`{parent}` is not an array"),
            };
            anyhow!("cannot resolve `{}`: {reason}", fmt_path(segs))
        })?;
    }
    Ok(cur)
}

// ───────────── 渲染 ─────────────

/// 值嵌入文本时的写法：字符串原样，null 为空，其余为 JSON。
fn stringify(value: &Value) -> Cow<'_, str> {
    match value {
        Value::String(text) => Cow::Borrowed(text),
        Value::Null => Cow::Borrowed(""),
        other => Cow::Owned(other.to_string()),
    }
}

pub fn render(tpl: &Template, ctx: &Value) -> Result<Value> {
    // 整串只有一个表达式时保留原类型（数字还是数字）
    if let [Part::Expr(expr)] = tpl.as_slice() {
        return eval(expr, ctx).cloned();
    }
    let mut out = String::new();
    for part in tpl {
        match part {
            Part::Text(text) => out.push_str(text),
            Part::Expr(expr) => out.push_str(&stringify(eval(expr, ctx)?)),
        }
    }
    Ok(Value::String(out))
}

/// 递归渲染整份配置里的所有字符串，返回新 Value
pub fn render_value(value: &Value, ctx: &Value) -> Result<Value> {
    Ok(match value {
        Value::String(text) => render(&parse_template(text)?, ctx)?,
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render_value(item, ctx))
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| Ok((key.clone(), render_value(item, ctx)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn path(segs: &[Seg]) -> Expr {
        Expr::Path(segs.to_vec())
    }

    fn key(name: &str) -> Seg {
        Seg::Key(name.to_string())
    }

    fn render_str(src: &str, ctx: &Value) -> Result<Value> {
        render(&parse_template(src)?, ctx)
    }

    #[test]
    fn parses_text_and_expressions_borrowing_the_source() {
        let parts = parse_template("a {{ x.y[2] }} b").unwrap();
        assert_eq!(
            parts,
            vec![
                Part::Text("a "),
                Part::Expr(path(&[key("x"), key("y"), Seg::Index(2)])),
                Part::Text(" b"),
            ]
        );
    }

    #[test]
    fn parse_errors_name_the_offending_expression() {
        let err = parse_template("x {{ a.. }} y").unwrap_err().to_string();
        assert!(err.contains("expected identifier"), "got: {err}");
        assert!(err.contains("{{a..}}"), "got: {err}");

        let err = parse_template("{{ a b }}").unwrap_err().to_string();
        assert!(err.contains("unexpected `b`"), "got: {err}");

        let err = parse_template("{{ a[x] }}").unwrap_err().to_string();
        assert!(err.contains("expected array index"), "got: {err}");

        let err = parse_template("{{ a[1 }}").unwrap_err().to_string();
        assert!(err.contains("expected `]`"), "got: {err}");

        let err = parse_template("{{ a").unwrap_err().to_string();
        assert!(err.contains("unclosed"), "got: {err}");
    }

    #[test]
    fn eval_borrows_the_value_from_the_context() {
        let ctx = json!({ "a": { "b": [10, 20] } });
        let value = eval(&parse_expr("a.b[1]").unwrap(), &ctx).unwrap();
        assert!(std::ptr::eq(value, &ctx["a"]["b"][1]));
    }

    #[test]
    fn unresolved_paths_say_where_resolution_stopped() {
        let ctx = json!({ "a": { "b": [1] }, "s": "text" });
        let err = |src: &str| render_str(src, &ctx).unwrap_err().to_string();

        assert_eq!(err("{{a.c}}"), "cannot resolve `a.c`: no key `c` in `a`");
        assert_eq!(
            err("{{a.b[3]}}"),
            "cannot resolve `a.b[3]`: index 3 out of range in `a.b`"
        );
        assert_eq!(err("{{s.x}}"), "cannot resolve `s.x`: `s` is not an object");
        assert_eq!(
            err("{{a[0]}}"),
            "cannot resolve `a[0]`: `a` is not an array"
        );
    }

    #[test]
    fn single_expression_keeps_the_value_type() {
        let ctx = json!({ "n": 7, "o": { "k": 1 } });
        assert_eq!(render_str("{{n}}", &ctx).unwrap(), json!(7));
        assert_eq!(render_str("{{o}}", &ctx).unwrap(), json!({ "k": 1 }));
    }

    #[test]
    fn mixed_templates_stringify_each_value() {
        let ctx = json!({ "s": "x", "n": 7, "z": null, "o": { "k": 1 } });
        assert_eq!(
            render_str("{{s}}-{{n}}-[{{z}}]-{{o}}", &ctx).unwrap(),
            json!(r#"x-7-[]-{"k":1}"#)
        );
    }

    #[test]
    fn render_value_walks_nested_structures() {
        let ctx = json!({ "v": "1" });
        let rendered = render_value(
            &json!({ "a": ["{{v}}", { "b": "x{{v}}" }], "n": 3, "t": true }),
            &ctx,
        )
        .unwrap();
        assert_eq!(
            rendered,
            json!({ "a": ["1", { "b": "x1" }], "n": 3, "t": true })
        );
    }
}
