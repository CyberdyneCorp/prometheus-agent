//! The security guidance rule set: heuristic patterns over written code,
//! each with the advice a finding carries. Findings are heuristics under
//! the declared trust model (spec: security-boundaries, Declared trust
//! model): they inform, they do not prove a vulnerability.

use std::sync::LazyLock;

use regex::Regex;

/// One rule.
pub struct Rule {
    /// Stable id (`py-pickle-loads`).
    pub id: &'static str,
    /// The class: unsafe deserialization, command injection, XSS sink,
    /// weak cryptography, XXE, missing subresource integrity, CI workflow
    /// injection, SQL injection.
    pub category: &'static str,
    /// What to do instead.
    pub guidance: &'static str,
    /// A match containing this text is not a finding (an `<script>` tag
    /// that already has `integrity`).
    unless: Option<&'static str>,
}

/// `(id, category, guidance, pattern, unless)`.
type Spec = (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
);

const DESER: &str = "unsafe deserialization";
const CMD: &str = "command injection";
const XSS: &str = "XSS sink";
const CRYPTO: &str = "weak cryptography";
const XXE: &str = "XXE";
const SRI: &str = "missing subresource integrity";
const CI: &str = "CI workflow injection";
const SQL: &str = "SQL injection";

const SPECS: &[Spec] = &[
    (
        "py-pickle-loads",
        DESER,
        "pickle runs code while loading: never unpickle data from a request, file or peer you do not control; use json or a schema-validated format.",
        r"\b(c?pickle|dill|shelve)\.loads?\(",
        None,
    ),
    (
        "py-yaml-load",
        DESER,
        "yaml.load with the default or full loader builds arbitrary objects: use yaml.safe_load.",
        r"\byaml\.(unsafe_)?load(_all)?\(",
        None,
    ),
    (
        "py-marshal-loads",
        DESER,
        "marshal is not safe for untrusted input: use json.",
        r"\bmarshal\.loads?\(",
        None,
    ),
    (
        "java-object-input-stream",
        DESER,
        "Java native deserialization of untrusted bytes leads to gadget-chain code execution: use a data format (JSON) or an ObjectInputFilter allowlist.",
        r"new\s+ObjectInputStream\s*\(",
        None,
    ),
    (
        "php-unserialize",
        DESER,
        "unserialize on user input allows object injection: use json_decode, or allowed_classes => false.",
        r"\bunserialize\s*\(\s*\$",
        None,
    ),
    (
        "ruby-marshal-load",
        DESER,
        "Marshal.load on untrusted data executes code: use JSON.parse.",
        r"\bMarshal\.load\(",
        None,
    ),
    (
        "node-serialize",
        DESER,
        "node-serialize evaluates functions in its input: use JSON.parse.",
        r"node-serialize|\.unserialize\(",
        None,
    ),
    (
        "py-os-system",
        CMD,
        "os.system runs a shell: pass an argument list to subprocess.run without shell=True and never format user input into a command line.",
        r"\bos\.(system|popen)\(",
        None,
    ),
    (
        "py-subprocess-shell",
        CMD,
        "shell=True hands the string to a shell: pass an argument list and keep shell=False.",
        r"subprocess\.\w+\([^)]*shell\s*=\s*True",
        None,
    ),
    (
        "py-eval-exec",
        CMD,
        "eval/exec run arbitrary code: parse data with json or ast.literal_eval.",
        r"(^|[^\w.])(eval|exec)\(\s*(request|input|data|payload|params|args|body)",
        None,
    ),
    (
        "node-child-process-exec",
        CMD,
        "child_process.exec runs a shell: use execFile/spawn with an argument array.",
        r"child_process\.exec(Sync)?\(|\bexecSync\(",
        None,
    ),
    (
        "js-new-function",
        CMD,
        "new Function compiles strings into code: avoid building code from data.",
        r"\bnew\s+Function\s*\(",
        None,
    ),
    (
        "go-exec-shell",
        CMD,
        "exec.Command(\"sh\", \"-c\", ...) runs a shell: call the program directly with separate arguments.",
        r#"exec\.Command(Context)?\([^)]*"(ba)?sh"\s*,\s*"-c""#,
        None,
    ),
    (
        "php-shell-exec",
        CMD,
        "shell functions with variables in the command allow injection: use escapeshellarg or avoid the shell.",
        r"\b(shell_exec|passthru|system|exec|popen)\s*\(\s*[^)]*\$",
        None,
    ),
    (
        "js-inner-html",
        XSS,
        "assigning innerHTML/outerHTML parses markup: use textContent, or sanitize with DOMPurify.",
        r"\.(inner|outer)HTML\s*\+?=",
        None,
    ),
    (
        "js-insert-adjacent-html",
        XSS,
        "insertAdjacentHTML parses markup: build nodes or sanitize first.",
        r"\.insertAdjacentHTML\s*\(",
        None,
    ),
    (
        "js-document-write",
        XSS,
        "document.write injects markup into the page: build DOM nodes instead.",
        r"\bdocument\.write(ln)?\s*\(",
        None,
    ),
    (
        "react-dangerously-set-inner-html",
        XSS,
        "dangerouslySetInnerHTML bypasses React's escaping: render text, or sanitize with DOMPurify.",
        r"dangerouslySetInnerHTML",
        None,
    ),
    (
        "vue-v-html",
        XSS,
        "v-html renders raw HTML: use text interpolation or sanitize.",
        r"\bv-html\s*=",
        None,
    ),
    (
        "template-mark-safe",
        XSS,
        "marking strings safe disables autoescaping: escape user data instead.",
        r"\|\s*safe\b|\bmark_safe\s*\(|\bMarkup\s*\(",
        None,
    ),
    (
        "weak-hash-md5",
        CRYPTO,
        "MD5 is broken for security uses: use SHA-256, and a password hash (argon2, bcrypt, scrypt) for passwords.",
        r#"(?i)\bhashlib\.md5\(|createHash\(\s*['"]md5['"]|\bMD5\.Create\(|MessageDigest\.getInstance\(\s*"MD5"|\bmd5\s*\("#,
        None,
    ),
    (
        "weak-hash-sha1",
        CRYPTO,
        "SHA-1 is broken for collision resistance: use SHA-256 or better.",
        r#"(?i)\bhashlib\.sha1\(|createHash\(\s*['"]sha1['"]|\bSHA1\.Create\(|MessageDigest\.getInstance\(\s*"SHA-?1""#,
        None,
    ),
    (
        "weak-cipher",
        CRYPTO,
        "DES, 3DES, RC4, Blowfish and ECB mode are weak: use AES-GCM or ChaCha20-Poly1305.",
        r#"\bMODE_ECB\b|/ECB/|\b(DES|DES3|TripleDES|RC4|ARC4|Blowfish)\b\s*[.(]|['"](des|rc4|des-ede3|bf)(-cbc)?['"]"#,
        None,
    ),
    (
        "insecure-random",
        CRYPTO,
        "Math.random and random.random are predictable: use crypto.getRandomValues / secrets for tokens and keys.",
        r"\b(token|secret|password|key|nonce|salt)\w*\s*=\s*.*(Math\.random\(\)|random\.random\(\))",
        None,
    ),
    (
        "tls-verification-disabled",
        CRYPTO,
        "disabling certificate verification allows interception: keep verification on and add the CA instead.",
        r"verify\s*=\s*False|rejectUnauthorized\s*:\s*false|InsecureSkipVerify\s*:\s*true|NODE_TLS_REJECT_UNAUTHORIZED",
        None,
    ),
    (
        "java-xxe",
        XXE,
        "XML parsers resolve external entities by default: disable DTDs (disallow-doctype-decl) before parsing untrusted XML.",
        r"\b(DocumentBuilderFactory|SAXParserFactory|XMLInputFactory)\.newInstance\(\)",
        None,
    ),
    (
        "py-xml-untrusted",
        XXE,
        "the standard library XML parsers are open to entity attacks: use defusedxml for untrusted XML.",
        r"\bfrom\s+xml\.(etree|dom|sax)\b|\bimport\s+xml\.(etree|dom|sax)\b|\blxml\.etree\.(parse|fromstring|XML)\(",
        None,
    ),
    (
        "php-libxml-noent",
        XXE,
        "LIBXML_NOENT expands external entities: drop it when parsing untrusted XML.",
        r"LIBXML_NOENT",
        None,
    ),
    (
        "html-script-without-sri",
        SRI,
        "a third-party script without integrity runs whatever the CDN serves: add integrity=\"sha384-...\" crossorigin=\"anonymous\".",
        r#"(?i)<script\b[^>]*\bsrc\s*=\s*["']https?://[^>]*>"#,
        Some("integrity"),
    ),
    (
        "html-stylesheet-without-sri",
        SRI,
        "a third-party stylesheet without integrity can be swapped: add integrity and crossorigin.",
        r#"(?i)<link\b[^>]*\bhref\s*=\s*["']https?://[^"']+\.css[^>]*>"#,
        Some("integrity"),
    ),
    (
        "gha-untrusted-expression",
        CI,
        "an attacker-controlled ${{ github.event.* }} value expanded in a run step is shell injection: pass it through env: and quote \"$VAR\".",
        r"\$\{\{\s*github\.(event\.(issue|pull_request|comment|review|review_comment|discussion|discussion_comment|head_commit|commits|pages)\b[^}]*|head_ref)\s*\}\}",
        None,
    ),
    (
        "gha-pull-request-target",
        CI,
        "pull_request_target runs with secrets on untrusted forks: never check out or run the PR's code in it.",
        r"\bpull_request_target\b",
        None,
    ),
    (
        "sql-string-building",
        SQL,
        "building SQL from strings allows injection: use parameterized queries.",
        r#"\b(execute|executemany|query|raw)\(\s*(f["']|["'][^"']*["']\s*(%|\+|\.format\())"#,
        None,
    ),
];

/// Every rule, compiled once.
pub static RULES: LazyLock<Vec<(Rule, Regex)>> = LazyLock::new(|| {
    SPECS
        .iter()
        .map(|&(id, category, guidance, pattern, unless)| {
            let regex = Regex::new(&format!("(?m){pattern}")).expect("valid guidance rule");
            (
                Rule {
                    id,
                    category,
                    guidance,
                    unless,
                },
                regex,
            )
        })
        .collect()
});

impl Rule {
    /// Whether `text` holds a finding of this rule.
    #[must_use]
    pub fn matches(&self, regex: &Regex, text: &str) -> bool {
        regex
            .find_iter(text)
            .any(|m| self.unless.is_none_or(|u| !m.as_str().contains(u)))
    }
}
