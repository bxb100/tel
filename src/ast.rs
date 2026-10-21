use serde_json::Value;
use std::{iter::Peekable, str::Chars};

// ───────────── AST ─────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Key(String),  // .name
    Index(usize), // [0]
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Path(Vec<Seg>), // steps.build.outputs.files[0]
                    // Literal(Value),                          // 'str' / 123 / true
                    // Not(Box<Expr>),
                    // Binary(Box<Expr>, Op, Box<Expr>),        // Op: Eq, Ne, And, Or
                    // Call(String, Vec<Expr>),                 // contains(...), format(...)
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    Expr(Expr),
}

pub type Template = Vec<Part>;

// ───────────── 解析：模板 → Vec<Part> ─────────────

pub fn parse_template(src: &str) -> Result<Template, String> {
    let mut parts = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("{{") {
        if i > 0 {
            parts.push(Part::Text(rest[..i].to_string()));
        }
        let after = &rest[i + 2..];
        let j = after.find("}}").ok_or("unclosed `{{`")?;
        parts.push(Part::Expr(parse_expr(&after[..j])?));
        rest = &after[j + 2..];
    }
    if !rest.is_empty() {
        parts.push(Part::Text(rest.to_string()));
    }
    Ok(parts)
}

// ───────────── 解析：`{{ }}` 内部 → Expr ─────────────

struct Parser<'a> {
    chars: Peekable<Chars<'a>>,
}

pub fn parse_expr(src: &str) -> Result<Expr, String> {
    let mut p = Parser {
        chars: src.chars().peekable(),
    };
    p.ws();
    let e = p.expr()?;
    p.ws();
    match p.chars.peek() {
        Some(c) => Err(format!("unexpected `{c}` in `{}`", src.trim())),
        None => Ok(e),
    }
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.chars.next_if(|c| c.is_whitespace()).is_some() {}
    }

    // 以后加运算符就在这一层做优先级处理: expr -> or -> and -> eq -> unary -> primary
    fn expr(&mut self) -> Result<Expr, String> {
        self.path()
    }

    fn ident(&mut self) -> Result<String, String> {
        let mut s = String::new();
        while let Some(c) = self
            .chars
            .next_if(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        {
            s.push(c);
        }
        if s.is_empty() {
            Err("expected identifier".into())
        } else {
            Ok(s)
        }
    }

    fn path(&mut self) -> Result<Expr, String> {
        let mut segs = vec![Seg::Key(self.ident()?)];
        loop {
            match self.chars.peek() {
                Some('.') => {
                    self.chars.next();
                    segs.push(Seg::Key(self.ident()?));
                }
                Some('[') => {
                    self.chars.next();
                    let n = self.ident()?;
                    let n = n.parse().map_err(|_| format!("bad index `{n}`"))?;
                    if self.chars.next() != Some(']') {
                        return Err("expected `]`".into());
                    }
                    segs.push(Seg::Index(n));
                }
                _ => break,
            }
        }
        Ok(Expr::Path(segs))
    }
}

// ───────────── 求值：Expr + ctx → Value ─────────────

pub fn eval(expr: &Expr, ctx: &Value) -> Result<Value, String> {
    match expr {
        Expr::Path(segs) => {
            let mut cur = ctx;
            for seg in segs {
                cur = match (seg, cur) {
                    (Seg::Key(k), Value::Object(m)) => m.get(k),
                    (Seg::Index(i), Value::Array(a)) => a.get(*i),
                    _ => None,
                }
                .ok_or_else(|| format!("cannot resolve {seg:?} in path {segs:?}"))?;
            }
            Ok(cur.clone())
        }
    }
}

// ───────────── 渲染 ─────────────

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn render(tpl: &Template, ctx: &Value) -> Result<Value, String> {
    // 整串只有一个表达式时保留原类型（数字还是数字）
    if let [Part::Expr(e)] = tpl.as_slice() {
        return eval(e, ctx);
    }
    let mut out = String::new();
    for p in tpl {
        match p {
            Part::Text(t) => out.push_str(t),
            Part::Expr(e) => out.push_str(&stringify(&eval(e, ctx)?)),
        }
    }
    Ok(Value::String(out))
}

/// 递归渲染整份配置里的所有字符串，返回新 Value
pub fn render_value(v: &Value, ctx: &Value) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) => render(&parse_template(s)?, ctx)?,
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|x| render_value(x, ctx))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| Ok((k.clone(), render_value(x, ctx)?)))
                .collect::<Result<_, String>>()?,
        ),
        other => other.clone(),
    })
}
