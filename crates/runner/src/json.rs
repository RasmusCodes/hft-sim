//! A minimal JSON writer (the workspace has no dependencies).

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum J {
    /// `null`.
    Null,
    /// A float; non-finite values are written as `null`.
    Num(f64),
    /// An integer, written exactly.
    Int(i128),
    /// A string.
    Str(String),
    /// An array.
    Arr(Vec<J>),
    /// An object with keys in insertion order.
    Obj(Vec<(String, J)>),
}

impl J {
    /// Object from (key, value) pairs.
    pub fn obj<const N: usize>(fields: [(&str, J); N]) -> J {
        J::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// Optional float.
    pub fn opt(x: Option<f64>) -> J {
        x.map_or(J::Null, J::Num)
    }

    /// Serialise.
    pub fn render(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }

    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Num(x) if x.is_finite() => out.push_str(&format!("{x}")),
            J::Num(_) => out.push_str("null"),
            J::Int(i) => out.push_str(&i.to_string()),
            J::Str(s) => {
                out.push('"');
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            J::Arr(v) => {
                out.push('[');
                for (k, x) in v.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    x.write(out);
                }
                out.push(']');
            }
            J::Obj(v) => {
                out.push('{');
                for (k, (key, x)) in v.iter().enumerate() {
                    if k > 0 {
                        out.push(',');
                    }
                    J::Str(key.clone()).write(out);
                    out.push(':');
                    x.write(out);
                }
                out.push('}');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_valid_json() {
        let v = J::obj([
            ("a", J::Int(-12)),
            ("b", J::Num(0.5)),
            ("c", J::Num(f64::NAN)),
            ("d", J::Str("q\"\\\n\u{1}".into())),
            ("e", J::Arr(vec![J::Num(-1.25), J::Null])),
        ]);
        assert_eq!(v.render(), r#"{"a":-12,"b":0.5,"c":null,"d":"q\"\\\n\u0001","e":[-1.25,null]}"#);
    }
}
