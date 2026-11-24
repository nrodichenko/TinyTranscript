//! The bits of Python behaviour the port has to reproduce exactly, because their output is stored
//! or compared: JSON text as `json.dumps` writes it (indent 1/2/none, `ensure_ascii`, float repr),
//! `round()`, `sum()` of floats, `str.split/strip/splitlines/casefold`, pathlib's `stem`/`suffix`,
//! truthiness and `==` on JSON values, and SHA-1 (`hashlib.sha1`).

use serde_json::Value;

// ------------------------------------------------------------------ numbers

/// `repr(float)`: shortest round-trip digits, exponent form below 1e-4 and from 1e16 (`1e-05`).
pub fn float_repr(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return if x.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let e = format!("{x:e}"); // shortest digits: "-1.5e-5", "1e16", "1.7954558913415108e9"
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let sign = if mant.starts_with('-') { "-" } else { "" };
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = digits.len() as i32;
    if (-4..16).contains(&exp) {
        let body = if exp >= 0 {
            let point = (exp + 1) as usize;
            if n <= exp + 1 {
                format!("{digits}{}.0", "0".repeat(point - n as usize))
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            }
        } else {
            format!("0.{}{digits}", "0".repeat((-exp - 1) as usize))
        };
        format!("{sign}{body}")
    } else {
        let m = if n > 1 { format!("{}.{}", &digits[..1], &digits[1..]) } else { digits };
        format!("{sign}{m}e{}{:02}", if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// `round(x, nd)` for a float: correctly rounded, ties to even (Rust's fixed formatting does the same).
pub fn round(x: f64, nd: usize) -> f64 {
    if !x.is_finite() {
        return x;
    }
    format!("{x:.nd$}").parse().unwrap_or(x)
}

/// `sum(floats)` as Python 3.12+ computes it (Neumaier compensated summation).
pub fn sum(xs: impl IntoIterator<Item = f64>) -> f64 {
    let (mut s, mut c) = (0.0f64, 0.0f64);
    for x in xs {
        let t = s + x;
        if s.abs() >= x.abs() {
            c += (s - t) + x;
        } else {
            c += (x - t) + s;
        }
        s = t;
    }
    if c != 0.0 && c.is_finite() {
        s += c;
    }
    s
}

/// `a // b` for integers (floor division).
pub fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) { q - 1 } else { q }
}

// ------------------------------------------------------------------ JSON text

/// `json.dumps(v, indent=indent, ensure_ascii=ensure_ascii)` (default separators).
pub fn dumps(v: &Value, indent: Option<usize>, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write_value(&mut out, v, indent, ensure_ascii, 0);
    out
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(n) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(n * level));
    }
}

fn write_value(out: &mut String, v: &Value, indent: Option<usize>, ascii: bool, level: usize) {
    let sep = if indent.is_some() { "," } else { ", " };
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                out.push_str(&float_repr(n.as_f64().unwrap_or(0.0)))
            } else {
                out.push_str(&n.to_string())
            }
        }
        Value::String(s) => write_str(out, s, ascii),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                newline(out, indent, level + 1);
                write_value(out, x, indent, ascii, level + 1);
            }
            newline(out, indent, level);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                newline(out, indent, level + 1);
                write_str(out, k, ascii);
                out.push_str(": ");
                write_value(out, x, indent, ascii, level + 1);
            }
            newline(out, indent, level);
            out.push('}');
        }
    }
}

fn write_str(out: &mut String, s: &str, ascii: bool) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if ascii && !(' '..='~').contains(&c) => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `str(value)` of a JSON value as Python would print it in an f-string.
pub fn str_of(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => if *b { "True" } else { "False" }.into(),
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => float_repr(n.as_f64().unwrap_or(0.0)),
        other => dumps(other, None, false),
    }
}

/// Python truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|x| x != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(m)) => !m.is_empty(),
    }
}

/// Python `==` on JSON values (ints and floats compare by value, dicts ignore key order).
pub fn eq(a: &Value, b: &Value) -> bool {
    fn num(v: &Value) -> Option<f64> {
        match v {
            Value::Number(n) => n.as_f64(),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if !x.is_f64() && !y.is_f64() => x == y,
        (Value::Number(_) | Value::Bool(_), Value::Number(_) | Value::Bool(_)) => num(a) == num(b),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eq(p, q)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| eq(v, w)))
        }
        _ => a == b,
    }
}

// ------------------------------------------------------------------ strings

/// `str.isspace()` for one character (Unicode White_Space plus the \x1c-\x1f separators).
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `s.split()`.
pub fn split(s: &str) -> Vec<&str> {
    s.split(is_space).filter(|w| !w.is_empty()).collect()
}

/// `s.strip()`.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `" ".join(s.split())`.
pub fn squash(s: &str) -> String {
    split(s).join(" ")
}

fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `s.splitlines(keepends)`.
pub fn splitlines(s: &str, keepends: bool) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if is_line_break(c) {
            let mut end = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = it.peek() {
                    it.next();
                    end = j + 1;
                }
            }
            out.push(if keepends { &s[start..end] } else { &s[start..i] });
            start = end;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `s.lower()`.
pub fn lower(s: &str) -> String {
    s.to_lowercase()
}

/// `s.casefold()` (lower case plus the full case foldings that differ from it).
pub fn casefold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.to_lowercase().chars() {
        match c {
            'ß' | 'ẞ' => out.push_str("ss"),
            'ς' => out.push('σ'),
            'ﬀ' => out.push_str("ff"),
            'ﬁ' => out.push_str("fi"),
            'ﬂ' => out.push_str("fl"),
            'ﬃ' => out.push_str("ffi"),
            'ﬄ' => out.push_str("ffl"),
            'ﬅ' | 'ﬆ' => out.push_str("st"),
            'ŉ' => out.push_str("ʼn"),
            'ſ' => out.push('s'),
            c => out.push(c),
        }
    }
    out
}

/// The first `n` characters (`s[:n]`, with Python's meaning of a negative `n`).
pub fn prefix(s: &str, n: i64) -> &str {
    let len = s.chars().count() as i64;
    let n = if n < 0 { (len + n).max(0) } else { n.min(len) };
    match s.char_indices().nth(n as usize) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// The last `n` characters (`s[-n:]`, n > 0).
pub fn suffix_chars(s: &str, n: usize) -> &str {
    let len = s.chars().count();
    if n >= len {
        return s;
    }
    match s.char_indices().nth(len - n) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

// ------------------------------------------------------------------ paths

/// pathlib `PurePath.stem` (Python 3.14).
pub fn stem(name: &str) -> &str {
    if let Some(i) = name.rfind('.') {
        let st = &name[..i];
        if !st.trim_start_matches('.').is_empty() {
            return st;
        }
    }
    name
}

/// pathlib `PurePath.suffix` (Python 3.14).
pub fn suffix(name: &str) -> &str {
    let n = name.trim_start_matches('.');
    match n.rfind('.') {
        Some(i) => &n[i..],
        None => "",
    }
}

/// `str(Path(s))`: duplicate slashes, `.` components and a trailing slash dropped.
pub fn norm_path(s: &str) -> String {
    let abs = s.starts_with('/');
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    let body = parts.join("/");
    match (abs, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => ".".into(),
        (false, false) => body,
    }
}

// ------------------------------------------------------------------ SHA-1

/// `hashlib.sha1(data).hexdigest()`.
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([chunk[4 * i], chunk[4 * i + 1], chunk[4 * i + 2], chunk[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A827999),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDC),
                _ => (b ^ c ^ d, 0xCA62C1D6),
            };
            let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floats_like_python() {
        // reference: repr() in Python 3.14
        assert_eq!(float_repr(-4.1e-05), "-4.1e-05");
        assert_eq!(float_repr(1e16), "1e+16");
        assert_eq!(float_repr(1e15), "1000000000000000.0");
        assert_eq!(float_repr(0.0001), "0.0001");
        assert_eq!(float_repr(1.5e-5), "1.5e-05");
        assert_eq!(float_repr(1795455891.3415108), "1795455891.3415108");
        assert_eq!(float_repr(123456789012345678.0), "1.2345678901234568e+17");
        assert_eq!(float_repr(8.0), "8.0");
        assert_eq!(float_repr(0.65), "0.65");
        assert_eq!(float_repr(-0.0), "-0.0");
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(float_repr(round(-0.04, 1)), "-0.0");
        // sum([0.1]*10) == 1.0 in Python 3.12+ (compensated), 0.9999999999999999 naively
        assert_eq!(sum(std::iter::repeat_n(0.1, 10)), 1.0);
        assert_eq!(floor_div(31, 2), 15);
        assert_eq!(floor_div(-3, 2), -2);
    }

    #[test]
    fn json_like_python() {
        let v = json!({"a": [1, 2.5, "Яш"], "b": {}, "c": [], "d": null, "e": true});
        assert_eq!(dumps(&v, None, false), r#"{"a": [1, 2.5, "Яш"], "b": {}, "c": [], "d": null, "e": true}"#);
        assert_eq!(dumps(&v, Some(1), false), "{\n \"a\": [\n  1,\n  2.5,\n  \"Яш\"\n ],\n \"b\": {},\n \"c\": [],\n \"d\": null,\n \"e\": true\n}");
        // json.dumps([["AB", "Анна 😀"]]) (ensure_ascii, surrogate pair)
        assert_eq!(dumps(&json!([["AB", "Анна 😀\u{7f}"]]), None, true),
                   r#"[["AB", "\u0410\u043d\u043d\u0430 \ud83d\ude00\u007f"]]"#);
        assert_eq!(dumps(&json!("a\"b\\c\n\u{1}"), None, false), r#""a\"b\\c\n\u0001""#);
    }

    #[test]
    fn strings_like_python() {
        assert_eq!(splitlines("a\r\nb\rc\n\nd", false), vec!["a", "b", "c", "", "d"]);
        assert_eq!(splitlines("a\r\nb\n", true), vec!["a\r\n", "b\n"]);
        assert_eq!(splitlines("", false), Vec::<&str>::new());
        assert_eq!(split(" a\u{1f}b\u{a0} c "), vec!["a", "b", "c"]);
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(prefix("абвг", 2), "аб");
        assert_eq!(prefix("абвг", -1), "абв");
        assert_eq!(suffix_chars("абвг", 2), "вг");
    }

    #[test]
    fn paths_like_python() {
        for (n, st, su) in [("a.b.m4a", "a.b", ".m4a"), ("name.", "name", "."), (".hidden", ".hidden", ""),
                            ("x.TXT", "x", ".TXT"), ("a..b", "a.", ".b"), ("..", "..", ""), ("a b.mp3", "a b", ".mp3")] {
            assert_eq!((stem(n), suffix(n)), (st, su), "{n}");
        }
        assert_eq!(norm_path("./a//b/"), "a/b");
        assert_eq!(norm_path("/x/./y"), "/x/y");
    }

    #[test]
    fn sha1_like_hashlib() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        // python3 -c 'import hashlib;print(hashlib.sha1(("Анна"*30).encode()).hexdigest()[:16])'
        assert_eq!(&sha1_hex("Анна".repeat(30).as_bytes())[..16], "623d9ddd0cca7ffd");
    }

    #[test]
    fn equality_like_python() {
        assert!(eq(&json!({"mtime": 1.5, "size": 3}), &json!({"size": 3.0, "mtime": 1.5})));
        assert!(!eq(&json!({"mtime": 1.5, "size": 3}), &json!(null)));
        assert!(truthy(Some(&json!(["x"]))));
        assert!(!truthy(Some(&json!([]))));
    }
}

#[cfg(test)]
mod parse_tests {
    #[test]
    fn floats_parse_exactly() {
        // without serde_json's float_roundtrip this parses one ulp off and every transcript looks changed
        let v: serde_json::Value = serde_json::from_str("[1795456363.2776747, 1795506275.0268037]").unwrap();
        assert_eq!(v[0].as_f64().unwrap(), 1795456363.0 + 277674661.0 * 1e-9);
        assert_eq!(v[1].as_f64().unwrap(), 1795506275.0 + 26803796.0 * 1e-9);
    }
}
