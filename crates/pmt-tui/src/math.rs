//! LaTeX math in answers (spec: tui, LaTeX math rendering): inline
//! `$...$` and block `$$...$$` become Unicode text (superscripts,
//! subscripts, fractions, Greek letters, common operators), dollar amounts
//! stay as they are, and math this renderer cannot format is shown as the
//! literal TeX in a code span. [`segments`] also splits out the answer's
//! own code spans and fenced blocks, whose text is never touched.

/// A run of an answer's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Plain text.
    Text(String),
    /// Formatted math.
    Math(String),
    /// Literal text shown as code (a code span, a fenced block, or math
    /// that could not be formatted).
    Code(String),
}

/// `text` split into plain text, formatted math and code.
#[must_use]
pub fn segments(text: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut fenced = false;
    let mut plain = String::new();
    for line in text.split_inclusive('\n') {
        let fence = line.trim_start().starts_with("```");
        if fenced || fence {
            flush(&mut out, &mut plain);
            push(&mut out, Segment::Code(line.to_owned()));
            fenced ^= fence;
        } else {
            plain.push_str(line);
        }
    }
    flush(&mut out, &mut plain);
    out
}

fn push(out: &mut Vec<Segment>, segment: Segment) {
    if matches!(&segment, Segment::Text(t) if t.is_empty()) {
        return;
    }
    match (out.last_mut(), &segment) {
        (Some(Segment::Text(a)), Segment::Text(b)) | (Some(Segment::Code(a)), Segment::Code(b)) => {
            a.push_str(b);
        }
        _ => out.push(segment),
    }
}

/// Splits unfenced `plain` into code spans, math and text.
fn flush(out: &mut Vec<Segment>, plain: &mut String) {
    let text = std::mem::take(plain);
    let mut rest = text.as_str();
    while !rest.is_empty() {
        let Some((at, found)) = next_special(rest) else {
            push(out, Segment::Text(rest.to_owned()));
            return;
        };
        push(out, Segment::Text(rest[..at].to_owned()));
        let (segment, used) = found;
        push(out, segment);
        rest = &rest[at + used..];
    }
}

/// The first code span or math in `text`: where it starts, what it is and
/// how many bytes it takes.
fn next_special(text: &str) -> Option<(usize, (Segment, usize))> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let found = match bytes[at] {
            b'`' => code_span(&text[at..]),
            b'$' if at > 0 && bytes[at - 1] == b'\\' => None,
            b'$' => math_at(&text[at..]),
            _ => None,
        };
        if let Some(found) = found {
            return Some((at, found));
        }
        at += 1;
    }
    None
}

fn code_span(text: &str) -> Option<(Segment, usize)> {
    let end = text[1..].find('`')? + 1;
    Some((Segment::Code(text[1..end].to_owned()), end + 1))
}

/// Math starting at `text[0] == '$'`: `$$...$$`, or `$...$` whose opening
/// `$` is followed and whose closing `$` is preceded by a non-space, the
/// closing one not followed by a digit (so `$5 and $10` is money).
fn math_at(text: &str) -> Option<(Segment, usize)> {
    if let Some(body) = text.strip_prefix("$$") {
        let end = body.find("$$")?;
        let tex = &body[..end];
        let used = end + 4;
        return Some((formatted(tex.trim(), &text[..used]), used));
    }
    let body = &text[1..];
    let first = body.chars().next()?;
    if first.is_whitespace() {
        return None;
    }
    let mut search = 0;
    while let Some(found) = body[search..].find('$') {
        let end = search + found;
        let before = body[..end].chars().next_back();
        let after = body[end + 1..].chars().next();
        let closes = end > 0
            && before.is_some_and(|c| !c.is_whitespace())
            && !after.is_some_and(|c| c.is_ascii_digit());
        if body[..end].contains('\n') {
            return None;
        }
        if closes {
            let used = end + 2;
            return Some((formatted(&body[..end], &text[..used]), used));
        }
        search = end + 1;
    }
    None
}

fn formatted(tex: &str, literal: &str) -> Segment {
    render(tex).map_or_else(
        |Unsupported| Segment::Code(literal.to_owned()),
        Segment::Math,
    )
}

/// TeX this renderer cannot format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unsupported;

/// `tex` as Unicode text.
///
/// # Errors
///
/// [`Unsupported`]: unknown macros, environments and unbalanced braces.
pub fn render(tex: &str) -> Result<String, Unsupported> {
    let chars: Vec<char> = tex.chars().collect();
    let mut parser = Parser { chars, at: 0 };
    let out = parser.sequence(false)?;
    if parser.at < parser.chars.len() {
        return Err(Unsupported);
    }
    Ok(out)
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    /// Atoms until the end, or the closing `}` of a group.
    fn sequence(&mut self, in_group: bool) -> Result<String, Unsupported> {
        let mut out = String::new();
        while let Some(c) = self.peek() {
            match c {
                '}' if in_group => return Ok(out),
                '}' => return Err(Unsupported),
                '^' | '_' => {
                    self.at += 1;
                    let arg = self.argument()?;
                    out.push_str(&script(&arg, c == '^'));
                }
                _ => out.push_str(&self.atom()?),
            }
        }
        if in_group { Err(Unsupported) } else { Ok(out) }
    }

    /// One `{group}`, macro or character.
    fn atom(&mut self) -> Result<String, Unsupported> {
        let c = self.peek().ok_or(Unsupported)?;
        self.at += 1;
        match c {
            '{' => {
                let inner = self.sequence(true)?;
                self.at += 1;
                Ok(inner)
            }
            '\\' => self.command(),
            '&' | '#' | '%' => Err(Unsupported),
            c => Ok(c.to_string()),
        }
    }

    /// A script's or a macro's argument: a group or one atom.
    fn argument(&mut self) -> Result<String, Unsupported> {
        while self.peek() == Some(' ') {
            self.at += 1;
        }
        self.atom()
    }

    fn command(&mut self) -> Result<String, Unsupported> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_alphabetic()) {
            self.at += 1;
        }
        if self.at == start {
            // `\,`, `\;`, `\ `, `\{` and friends: one character.
            let c = self.peek().ok_or(Unsupported)?;
            self.at += 1;
            return match c {
                ',' | ';' | ':' | ' ' | '!' => Ok(" ".to_owned()),
                '{' | '}' | '$' | '%' | '_' | '&' | '#' => Ok(c.to_string()),
                _ => Err(Unsupported),
            };
        }
        let name: String = self.chars[start..self.at].iter().collect();
        self.named(&name)
    }

    fn named(&mut self, name: &str) -> Result<String, Unsupported> {
        if let Some(symbol) = symbol(name) {
            return Ok(symbol.to_owned());
        }
        match name {
            "frac" | "dfrac" | "tfrac" => {
                let top = self.argument()?;
                let bottom = self.argument()?;
                Ok(fraction(&top, &bottom))
            }
            "sqrt" => {
                let arg = self.argument()?;
                Ok(format!("√{}", grouped(&arg)))
            }
            "text" | "mathrm" | "mathit" | "mathbf" | "operatorname" | "textrm" => self.argument(),
            "mathbb" => {
                let arg = self.argument()?;
                Ok(arg.chars().map(double_struck).collect())
            }
            "left" | "right" => {
                let delimiter = self.argument()?;
                Ok(if delimiter == "." {
                    String::new()
                } else {
                    delimiter
                })
            }
            "quad" | "qquad" => Ok("  ".to_owned()),
            _ => Err(Unsupported),
        }
    }
}

/// `x` wrapped in parentheses when it is more than one symbol.
fn grouped(x: &str) -> String {
    if x.chars().count() <= 1 || x.chars().all(char::is_alphanumeric) {
        x.to_owned()
    } else {
        format!("({x})")
    }
}

fn fraction(top: &str, bottom: &str) -> String {
    let vulgar = match (top, bottom) {
        ("1", "2") => Some("½"),
        ("1", "3") => Some("⅓"),
        ("2", "3") => Some("⅔"),
        ("1", "4") => Some("¼"),
        ("3", "4") => Some("¾"),
        _ => None,
    };
    vulgar.map_or_else(
        || format!("{}/{}", grouped(top), grouped(bottom)),
        str::to_owned,
    )
}

/// A super- or subscript: Unicode script characters when every character
/// has one, else `^(...)` / `_(...)`.
fn script(arg: &str, sup: bool) -> String {
    let mapped: Option<String> = arg
        .chars()
        .map(|c| if sup { superscript(c) } else { subscript(c) })
        .collect();
    mapped.unwrap_or_else(|| {
        let mark = if sup { '^' } else { '_' };
        if arg.chars().count() > 1 {
            format!("{mark}({arg})")
        } else {
            format!("{mark}{arg}")
        }
    })
}

fn superscript(c: char) -> Option<char> {
    let from = "0123456789+-=()niax";
    let to = "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿⁱᵃˣ";
    from.chars()
        .position(|f| f == c)
        .and_then(|i| to.chars().nth(i))
}

fn subscript(c: char) -> Option<char> {
    let from = "0123456789+-=()aeijnox";
    let to = "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎ₐₑᵢⱼₙₒₓ";
    from.chars()
        .position(|f| f == c)
        .and_then(|i| to.chars().nth(i))
}

fn double_struck(c: char) -> char {
    match c {
        'R' => 'ℝ',
        'N' => 'ℕ',
        'Z' => 'ℤ',
        'Q' => 'ℚ',
        'C' => 'ℂ',
        other => other,
    }
}

/// Greek letters and operators.
fn symbol(name: &str) -> Option<&'static str> {
    const SYMBOLS: &[(&str, &str)] = &[
        ("alpha", "α"),
        ("beta", "β"),
        ("gamma", "γ"),
        ("delta", "δ"),
        ("epsilon", "ε"),
        ("varepsilon", "ε"),
        ("zeta", "ζ"),
        ("eta", "η"),
        ("theta", "θ"),
        ("iota", "ι"),
        ("kappa", "κ"),
        ("lambda", "λ"),
        ("mu", "μ"),
        ("nu", "ν"),
        ("xi", "ξ"),
        ("pi", "π"),
        ("rho", "ρ"),
        ("sigma", "σ"),
        ("tau", "τ"),
        ("phi", "φ"),
        ("varphi", "φ"),
        ("chi", "χ"),
        ("psi", "ψ"),
        ("omega", "ω"),
        ("Gamma", "Γ"),
        ("Delta", "Δ"),
        ("Theta", "Θ"),
        ("Lambda", "Λ"),
        ("Pi", "Π"),
        ("Sigma", "Σ"),
        ("Phi", "Φ"),
        ("Psi", "Ψ"),
        ("Omega", "Ω"),
        ("times", "×"),
        ("cdot", "·"),
        ("div", "÷"),
        ("pm", "±"),
        ("mp", "∓"),
        ("le", "≤"),
        ("leq", "≤"),
        ("ge", "≥"),
        ("geq", "≥"),
        ("neq", "≠"),
        ("ne", "≠"),
        ("approx", "≈"),
        ("equiv", "≡"),
        ("sim", "∼"),
        ("propto", "∝"),
        ("infty", "∞"),
        ("sum", "∑"),
        ("prod", "∏"),
        ("int", "∫"),
        ("oint", "∮"),
        ("partial", "∂"),
        ("nabla", "∇"),
        ("to", "→"),
        ("rightarrow", "→"),
        ("leftarrow", "←"),
        ("Rightarrow", "⇒"),
        ("Leftarrow", "⇐"),
        ("leftrightarrow", "↔"),
        ("iff", "⇔"),
        ("implies", "⇒"),
        ("mapsto", "↦"),
        ("in", "∈"),
        ("notin", "∉"),
        ("subset", "⊂"),
        ("subseteq", "⊆"),
        ("supset", "⊃"),
        ("cup", "∪"),
        ("cap", "∩"),
        ("emptyset", "∅"),
        ("forall", "∀"),
        ("exists", "∃"),
        ("neg", "¬"),
        ("land", "∧"),
        ("lor", "∨"),
        ("cdots", "⋯"),
        ("ldots", "…"),
        ("dots", "…"),
        ("circ", "∘"),
        ("degree", "°"),
        ("prime", "′"),
        ("langle", "⟨"),
        ("rangle", "⟩"),
        ("log", "log"),
        ("ln", "ln"),
        ("exp", "exp"),
        ("sin", "sin"),
        ("cos", "cos"),
        ("tan", "tan"),
        ("lim", "lim"),
        ("max", "max"),
        ("min", "min"),
    ];
    SYMBOLS.iter().find(|(n, _)| *n == name).map(|(_, s)| *s)
}

#[cfg(test)]
mod tests {
    use super::{Segment, Unsupported, render, segments};

    fn math(s: &str) -> Segment {
        Segment::Math(s.to_owned())
    }

    fn text(s: &str) -> Segment {
        Segment::Text(s.to_owned())
    }

    #[test]
    fn inline_math_becomes_unicode() {
        assert_eq!(render(r"x^2 + y_1"), Ok("x² + y₁".to_owned()));
        assert_eq!(render(r"\frac{1}{2}"), Ok("½".to_owned()));
        assert_eq!(render(r"\frac{a+b}{c}"), Ok("(a+b)/c".to_owned()));
        assert_eq!(
            render(r"\alpha \leq \beta \times \pi"),
            Ok("α ≤ β × π".to_owned())
        );
        assert_eq!(render(r"\sqrt{x}"), Ok("√x".to_owned()));
        assert_eq!(render(r"e^{i\pi}"), Ok("e^(iπ)".to_owned()));
        assert_eq!(render(r"\sum_{i=0}^{n} i"), Ok("∑ᵢ₌₀ⁿ i".to_owned()));
        assert_eq!(render(r"\mathbb{R}^3"), Ok("ℝ³".to_owned()));
    }

    #[test]
    fn answers_split_into_text_math_and_code() {
        assert_eq!(
            segments("so $E = mc^2$ holds"),
            [text("so "), math("E = mc²"), text(" holds")]
        );
        assert_eq!(
            segments("$$\\int_0^1 x\\,dx = \\frac{1}{2}$$"),
            [math("∫₀¹ x dx = ½")]
        );
    }

    #[test]
    fn dollar_amounts_are_left_alone() {
        for money in [
            "it costs $5 and $10 more",
            "pay $5 or $6.",
            "a $ sign",
            "US$ 3",
        ] {
            assert_eq!(segments(money), [text(money)], "{money}");
        }
    }

    /// Scenario: Unsupported macro.
    #[test]
    fn unsupported_math_is_the_literal_tex_in_a_code_span() {
        assert_eq!(
            segments(r"see $\begin{tikzpicture}$ here"),
            [
                text("see "),
                Segment::Code(r"$\begin{tikzpicture}$".to_owned()),
                text(" here")
            ]
        );
        assert_eq!(render(r"\frac{1}"), Err(Unsupported));
    }

    #[test]
    fn code_spans_and_fences_are_never_formatted() {
        assert_eq!(
            segments("run `echo $x^2$` now"),
            [
                text("run "),
                Segment::Code("echo $x^2$".to_owned()),
                text(" now")
            ]
        );
        assert_eq!(
            segments("```\n$x^2$\n```\nafter $a_1$"),
            [
                Segment::Code("```\n$x^2$\n```\n".to_owned()),
                text("after "),
                math("a₁")
            ]
        );
    }
}
