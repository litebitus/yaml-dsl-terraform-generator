//! `fn.format`: a layout and its operands, with Go's printing verbs and tfgen's own.

/// Writes `operands` into `layout`. An operand is a scalar's text; a number verb
/// reads it as a number.
pub fn format_layout(layout: &str, operands: &[String]) -> Result<String, String> {
    let mut written = String::new();
    let mut operands = operands.iter();
    let mut characters = layout.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '%' {
            written.push(character);
            continue;
        }
        let mut directive = Directive::default();
        while let Some(&flag) = characters.peek() {
            match flag {
                '-' => directive.left_aligned = true,
                '0' => directive.zero_padded = true,
                '+' => directive.signed = true,
                ' ' | '#' => {}
                _ => break,
            }
            characters.next();
        }
        directive.width = digits(&mut characters);
        if characters.peek() == Some(&'.') {
            characters.next();
            directive.precision = Some(digits(&mut characters).unwrap_or(0));
        }
        let verb = characters
            .next()
            .ok_or_else(|| format!("layout `{layout}` ends inside a verb"))?;
        if verb == '%' {
            written.push('%');
            continue;
        }
        let operand = operands
            .next()
            .ok_or_else(|| format!("layout `{layout}` has more verbs than operands"))?;
        let text = verb_text(verb, operand, &directive)
            .map_err(|message| format!("layout `{layout}`: {message}"))?;
        written.push_str(&directive.padded(text));
    }
    if operands.next().is_some() {
        return Err(format!("layout `{layout}` has more operands than verbs"));
    }
    Ok(written)
}

#[derive(Default)]
struct Directive {
    left_aligned: bool,
    zero_padded: bool,
    signed: bool,
    width: Option<usize>,
    precision: Option<usize>,
}

impl Directive {
    fn padded(&self, text: String) -> String {
        let length = text.chars().count();
        let Some(width) = self.width.filter(|&width| width > length) else {
            return text;
        };
        let padding = width - length;
        if self.left_aligned {
            format!("{text}{}", " ".repeat(padding))
        } else if self.zero_padded {
            match text.strip_prefix('-') {
                Some(digits) => format!("-{}{digits}", "0".repeat(padding)),
                None => format!("{}{text}", "0".repeat(padding)),
            }
        } else {
            format!("{}{text}", " ".repeat(padding))
        }
    }

    fn truncated(&self, text: &str) -> String {
        match self.precision {
            Some(precision) => text.chars().take(precision).collect(),
            None => text.to_owned(),
        }
    }
}

fn digits(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<usize> {
    let mut number = None;
    while let Some(digit) = characters
        .peek()
        .and_then(|character| character.to_digit(10))
    {
        number = Some(number.unwrap_or(0) * 10 + digit as usize);
        characters.next();
    }
    number
}

fn verb_text(verb: char, operand: &str, directive: &Directive) -> Result<String, String> {
    Ok(match verb {
        's' | 'v' => directive.truncated(operand),
        'u' => directive.truncated(&operand.to_uppercase()),
        'l' => directive.truncated(&operand.to_lowercase()),
        'A' => directive.truncated(&operand.replace('/', "_")),
        'B' => {
            let precision = directive
                .precision
                .filter(|&precision| precision > 0)
                .ok_or("`%B` takes a precision, such as `%.64B`")?;
            let length = operand.chars().count();
            if length <= precision {
                operand.to_owned()
            } else {
                let tail: String = operand.chars().skip(length - (precision - 1)).collect();
                format!("-{tail}")
            }
        }
        'q' => quoted(&directive.truncated(operand)),
        'd' => {
            let number: i128 = operand
                .parse()
                .map_err(|_| format!("`%d` takes an integer, not `{operand}`"))?;
            if directive.signed && number >= 0 {
                format!("+{number}")
            } else {
                number.to_string()
            }
        }
        'x' | 'X' => {
            let hexadecimal = match operand.parse::<i128>() {
                Ok(number) => format!("{number:x}"),
                Err(_) => operand.bytes().map(|byte| format!("{byte:02x}")).collect(),
            };
            if verb == 'X' {
                hexadecimal.to_uppercase()
            } else {
                hexadecimal
            }
        }
        'f' => {
            let number: f64 = operand
                .parse()
                .map_err(|_| format!("`%f` takes a number, not `{operand}`"))?;
            format!(
                "{number:.precision$}",
                precision = directive.precision.unwrap_or(6)
            )
        }
        other => return Err(format!("`%{other}` is not a verb fn.format writes")),
    })
}

fn quoted(text: &str) -> String {
    let mut quoted = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\t' => quoted.push_str("\\t"),
            '\r' => quoted.push_str("\\r"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(layout: &str, operands: &[&str]) -> Result<String, String> {
        let operands: Vec<String> = operands.iter().map(|text| (*text).to_owned()).collect();
        format_layout(layout, &operands)
    }

    #[test]
    fn go_verbs_write_with_width_and_precision() {
        assert_eq!(
            format("%s-%05d|%-4s|", &["a", "42", "b"]).unwrap(),
            "a-00042|b   |"
        );
        assert_eq!(
            format("%.2s %q %x %.1f", &["abc", "x\"", "255", "2.25"]).unwrap(),
            "ab \"x\\\"\" ff 2.2"
        );
        assert_eq!(format("100%%", &[]).unwrap(), "100%");
    }

    #[test]
    fn tfgen_verbs_case_slash_and_cut_from_the_left() {
        assert_eq!(format("%u %l", &["mock", "MOCK"]).unwrap(), "MOCK mock");
        assert_eq!(
            format("mock_%A", &["k8s/apps/build"]).unwrap(),
            "mock_k8s_apps_build"
        );
        assert_eq!(format("%.8B", &["abcdefgh"]).unwrap(), "abcdefgh");
        assert_eq!(format("%.8B", &["abcdefghij"]).unwrap(), "-defghij");
    }

    #[test]
    fn a_mismatch_of_verbs_and_operands_is_an_error() {
        assert!(format("%s %s", &["a"]).is_err());
        assert!(format("%s", &["a", "b"]).is_err());
        assert!(format("%B", &["a"]).is_err());
        assert!(format("%d", &["a"]).is_err());
    }
}
