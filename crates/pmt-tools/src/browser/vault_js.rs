//! The page scripts `browser_vault` runs to put vault values into form
//! fields. Each is one expression called with its values as JSON
//! arguments; it sets a field only when its value differs (so a page that
//! already holds the identifier counts only the password) and answers the
//! number of fields it filled. Values go into the page and nowhere else.

/// Shared helpers: visible fields and a value setter that React-style
/// controlled inputs notice.
const PRELUDE: &str = r"
const visible = (el) => el && !el.disabled && el.type !== 'hidden' &&
  !!(el.offsetWidth || el.offsetHeight || el.getClientRects().length);
const fields = () => [...document.querySelectorAll('input, select, textarea')].filter(visible);
const put = (el, value) => {
  if (!el || value === undefined || value === null || value === '' || el.value === value) return 0;
  el.focus();
  const proto = Object.getPrototypeOf(el);
  const setter = Object.getOwnPropertyDescriptor(proto, 'value');
  if (setter && setter.set) { setter.set.call(el, value); } else { el.value = value; }
  el.dispatchEvent(new Event('input', { bubbles: true }));
  el.dispatchEvent(new Event('change', { bubbles: true }));
  return 1;
};
const hint = (el) => `${el.autocomplete || ''} ${el.name || ''} ${el.id || ''} ${el.placeholder || ''}`.toLowerCase();
const pick = (list, auto, pattern) =>
  list.find((el) => (el.autocomplete || '').toLowerCase().split(/\s+/).includes(auto)) ||
  list.find((el) => pattern.test(hint(el)));
";

/// `(identifier, password)`: the password field and the identifier field
/// before it.
pub(super) fn login(identifier: &str, password: &str) -> String {
    call(
        r"(id, pw) => {
  const all = fields().filter((el) => el.tagName === 'INPUT');
  const pass = all.find((el) => el.type === 'password');
  if (!pass) return 0;
  const before = all.slice(0, all.indexOf(pass)).reverse();
  const user = pick(all, 'username', /user|email|login/) ||
    before.find((el) => ['text', 'email', 'tel'].includes(el.type));
  return put(user, id) + put(pass, pw);
}",
        &[identifier, password],
    )
}

/// `(holder, number, expiry, cvc)` into `cc-*` fields (or fields named
/// like them); `MM/YY` also fills separate month and year fields.
pub(super) fn card(holder: &str, number: &str, expiry: &str, cvc: &str) -> String {
    call(
        r"(holder, number, expiry, cvc) => {
  const all = fields();
  const [month, year] = expiry.split('/');
  let n = put(pick(all, 'cc-name', /card.?holder|name.?on.?card|cc.?name/), holder);
  n += put(pick(all, 'cc-number', /card.?number|cc.?num|cardnumber/), number);
  const exp = pick(all, 'cc-exp', /^\s*(cc-?exp|exp(iry|iration)?(.?date)?)\s*$|expiry|expiration/);
  if (exp) { n += put(exp, expiry); } else {
    n += put(pick(all, 'cc-exp-month', /exp.?month|cc.?month/), month);
    n += put(pick(all, 'cc-exp-year', /exp.?year|cc.?year/), year);
  }
  n += put(pick(all, 'cc-csc', /cvc|cvv|csc|security.?code/), cvc);
  return n;
}",
        &[holder, number, expiry, cvc],
    )
}

/// An address into `name` / `address-*` / `postal-code` / `country`
/// fields.
pub(super) fn address(values: &[&str; 7]) -> String {
    call(
        r"(name, line1, line2, city, region, postal, country) => {
  const all = fields();
  return put(pick(all, 'name', /full.?name|^\s*name\b/), name) +
    put(pick(all, 'address-line1', /address.?(line)?.?1|street/), line1) +
    put(pick(all, 'address-line2', /address.?(line)?.?2|apartment|suite/), line2) +
    put(pick(all, 'address-level2', /city|town|locality/), city) +
    put(pick(all, 'address-level1', /state|province|region/), region) +
    put(pick(all, 'postal-code', /postal|zip|postcode/), postal) +
    put(pick(all, 'country', /country/), country);
}",
        values,
    )
}

/// A one-time code into the code field (`one-time-code`, or a field named
/// like one, or the only text field on the page).
pub(super) fn code(code: &str) -> String {
    call(
        r"(code) => {
  const all = fields().filter((el) => el.tagName === 'INPUT');
  const texts = all.filter((el) => ['text', 'tel', 'number', ''].includes(el.type));
  const field = pick(all, 'one-time-code', /otp|one.?time|totp|2fa|mfa|verification|auth.?code|\bcode\b/) ||
    (texts.length === 1 ? texts[0] : null);
  return put(field, code);
}",
        &[code],
    )
}

fn call(function: &str, args: &[&str]) -> String {
    let args: Vec<String> = args
        .iter()
        .map(|a| serde_json::Value::String((*a).to_owned()).to_string())
        .collect();
    format!(
        "(() => {{ {PRELUDE}\nreturn ({function})({}); }})()",
        args.join(", ")
    )
}
