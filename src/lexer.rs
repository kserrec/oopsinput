//! Conservative shell lexer (SPEC §13): words, quoting state, control
//! operators, redirections, and opaque spans for anything that cannot be
//! analyzed without evaluation — command/process substitution, backticks,
//! arithmetic, heredocs, zsh glob qualifiers. It never expands anything,
//! never invokes a shell, and never panics on any input (fuzz-smoked).
//! Constructs it can't see through produce honest uncertainty evidence
//! codes, not guesses.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    Word(Word),
    /// Control operator: `&&` `||` `|` `|&` `;` `;;` `&` `(` `)`
    Op(&'static str),
    /// Redirection operator with any fd prefix, e.g. `>` `2>>` `<<<` `&>`
    Redirect(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    /// Literal text with quoting resolved where that is safe; opaque spans
    /// (substitutions, expansions) are kept as their raw source text.
    pub text: String,
    /// Some part of the word was quoted or backslash-escaped.
    pub quoted: bool,
    /// Contains an expansion, substitution, or glob: the runtime value is
    /// unknowable without evaluation, which we never do.
    pub expands: bool,
}

pub struct Lexed {
    pub tokens: Vec<Token>,
    /// Stable evidence codes for constructs we can't analyze (SPEC §13),
    /// deduplicated, in first-seen order.
    pub uncertainty: Vec<&'static str>,
}

/// The characters zsh itself treats as word separators: space, tab, newline.
/// NOT `char::is_whitespace()` — Unicode blanks like the no-break space are
/// literal word characters to the shell, and this lexer must tokenize the way
/// the shell will, or the typo layer can analyze (and "correct") a different
/// word than the one the shell resolved. Regression (bughunt 2026-08-06):
/// `\u{A0}gti` was analyzed as `gti`, and consenting to the correction ran
/// `\u{A0}git` — another command-not-found.
pub fn is_shell_whitespace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n')
}

pub fn lex(input: &str) -> Lexed {
    let mut lx = Lexer {
        chars: input.chars().collect(),
        i: 0,
        tokens: Vec::new(),
        uncertainty: Vec::new(),
    };
    lx.run();
    Lexed {
        tokens: lx.tokens,
        uncertainty: lx.uncertainty,
    }
}

/// Words in command position: the first non-assignment word of each command
/// segment (segments are split by control operators). Redirect targets are
/// skipped. Conservative: a word containing an expansion still counts —
/// callers must check `expands` before trusting `text` as literal.
pub fn command_words(lexed: &Lexed) -> Vec<&Word> {
    let mut out = Vec::new();
    let mut at_start = true;
    let mut redirect_target = false;
    for t in &lexed.tokens {
        match t {
            Token::Op(_) => {
                at_start = true;
                redirect_target = false;
            }
            Token::Redirect(_) => redirect_target = true,
            Token::Word(w) => {
                if redirect_target {
                    redirect_target = false;
                } else if at_start && !is_assignment(w) {
                    out.push(w);
                    at_start = false;
                }
            }
        }
    }
    out
}

/// `NAME=...` / `NAME+=...` shape. Quoted words are never assignments.
pub(crate) fn is_assignment(w: &Word) -> bool {
    if w.quoted {
        return false;
    }
    let Some(eq) = w.text.find('=') else {
        return false;
    };
    // '=' is ASCII, so `eq` is a char boundary and the slice cannot panic.
    let name = w.text[..eq].strip_suffix('+').unwrap_or(&w.text[..eq]);
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

struct Lexer {
    chars: Vec<char>,
    i: usize,
    tokens: Vec<Token>,
    uncertainty: Vec<&'static str>,
}

impl Lexer {
    fn cur(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    fn peek(&self, n: usize) -> Option<char> {
        self.chars.get(self.i + n).copied()
    }

    fn note(&mut self, code: &'static str) {
        if !self.uncertainty.contains(&code) {
            self.uncertainty.push(code);
        }
    }

    // Every arm below either advances `i` or dispatches to a method that
    // does, so the loop always terminates: `i` is strictly increasing.
    fn run(&mut self) {
        while let Some(c) = self.cur() {
            match c {
                c if is_shell_whitespace(c) => self.i += 1,
                '&' | '|' | ';' => self.lex_control(c),
                '<' | '>' if self.peek(1) == Some('(') => self.lex_word(), // <(cmd)
                '<' | '>' => self.lex_redirect(0),
                '(' => {
                    self.i += 1;
                    self.tokens.push(Token::Op("("));
                }
                ')' => {
                    self.i += 1;
                    self.tokens.push(Token::Op(")"));
                }
                '0'..='9' => {
                    // digits directly attached to < or > at token start are
                    // an fd prefix (`2>err`), otherwise an ordinary word
                    let mut n = 1;
                    while matches!(self.peek(n), Some('0'..='9')) {
                        n += 1;
                    }
                    if matches!(self.peek(n), Some('<' | '>')) {
                        self.lex_redirect(n);
                    } else {
                        self.lex_word();
                    }
                }
                _ => self.lex_word(),
            }
        }
    }

    fn lex_control(&mut self, c: char) {
        match c {
            '&' => match self.peek(1) {
                Some('&') => {
                    self.i += 2;
                    self.tokens.push(Token::Op("&&"));
                }
                Some('>') => self.lex_redirect(0), // &> / &>> both-streams redirect
                _ => {
                    self.i += 1;
                    self.tokens.push(Token::Op("&"));
                }
            },
            '|' => match self.peek(1) {
                Some('|') => {
                    self.i += 2;
                    self.tokens.push(Token::Op("||"));
                }
                Some('&') => {
                    self.i += 2;
                    self.tokens.push(Token::Op("|&"));
                }
                _ => {
                    self.i += 1;
                    self.tokens.push(Token::Op("|"));
                }
            },
            _ => {
                if self.peek(1) == Some(';') {
                    self.i += 2;
                    self.tokens.push(Token::Op(";;"));
                } else {
                    self.i += 1;
                    self.tokens.push(Token::Op(";"));
                }
            }
        }
    }

    /// Consume one redirection operator; `digits` fd-prefix chars sit at the
    /// cursor (0 when the operator starts directly with `<` `>` `&`).
    fn lex_redirect(&mut self, digits: usize) {
        let mut op = String::new();
        for _ in 0..digits {
            if let Some(d) = self.cur() {
                op.push(d);
                self.i += 1;
            }
        }
        let Some(first) = self.cur() else {
            return; // unreachable by dispatch; drop gracefully
        };
        op.push(first);
        self.i += 1;
        match first {
            '<' => match self.cur() {
                Some('<') => {
                    op.push('<');
                    self.i += 1;
                    if self.cur() == Some('<') {
                        op.push('<'); // <<< herestring — target is a word
                        self.i += 1;
                    } else {
                        if self.cur() == Some('-') {
                            op.push('-'); // <<-
                            self.i += 1;
                        }
                        // Heredoc body lives on later PS2 lines we never see.
                        self.note("syntax.heredoc");
                    }
                }
                Some(c @ ('&' | '>')) => {
                    op.push(c); // <& <>
                    self.i += 1;
                }
                _ => {}
            },
            '>' => match self.cur() {
                Some('>') => {
                    op.push('>');
                    self.i += 1;
                    if self.cur() == Some('&') {
                        op.push('&'); // >>&
                        self.i += 1;
                    }
                }
                Some(c @ ('&' | '|')) => {
                    op.push(c); // >& >|
                    self.i += 1;
                }
                _ => {}
            },
            // dispatched from '&>': cursor now on '>'
            _ => {
                if self.cur() == Some('>') {
                    op.push('>');
                    self.i += 1;
                    if self.cur() == Some('>') {
                        op.push('>'); // &>>
                        self.i += 1;
                    }
                }
            }
        }
        self.tokens.push(Token::Redirect(op));
    }

    fn lex_word(&mut self) {
        let mut w = Word::default();
        let mut has = false;
        while let Some(c) = self.cur() {
            match c {
                c if is_shell_whitespace(c) => break,
                '&' | '|' | ';' | ')' => break,
                '<' | '>' => {
                    if self.peek(1) == Some('(') {
                        // process substitution glues to the word: <(...) >(...)
                        w.text.push(c);
                        self.i += 1;
                        w.expands = true;
                        self.note("syntax.opaque_substitution");
                        self.consume_paren_group(&mut w);
                        has = true;
                        continue;
                    }
                    break;
                }
                '\'' => {
                    self.i += 1;
                    w.quoted = true;
                    has = true;
                    self.consume_single_quote(&mut w);
                }
                '"' => {
                    self.i += 1;
                    w.quoted = true;
                    has = true;
                    self.consume_double_quote(&mut w);
                }
                '\\' => {
                    self.i += 1;
                    w.quoted = true;
                    if let Some(n) = self.cur() {
                        w.text.push(n);
                        self.i += 1;
                        has = true;
                    }
                }
                '$' => {
                    has = true;
                    self.consume_dollar(&mut w, false);
                }
                '`' => {
                    has = true;
                    self.consume_backtick(&mut w);
                }
                '(' => {
                    // mid-word bare paren: zsh glob qualifier / extended glob
                    w.expands = true;
                    self.note("syntax.zsh_glob");
                    self.consume_paren_group(&mut w);
                    has = true;
                }
                '*' | '?' | '[' | '~' | '^' | '#' => {
                    w.expands = true;
                    w.text.push(c);
                    self.i += 1;
                    has = true;
                }
                '=' if !has => {
                    w.expands = true; // zsh `=cmd` path expansion at word start
                    w.text.push(c);
                    self.i += 1;
                    has = true;
                }
                _ => {
                    w.text.push(c);
                    self.i += 1;
                    has = true;
                }
            }
        }
        if has {
            self.tokens.push(Token::Word(w));
        }
    }

    fn consume_single_quote(&mut self, w: &mut Word) {
        while let Some(c) = self.cur() {
            self.i += 1;
            if c == '\'' {
                return;
            }
            w.text.push(c);
        }
        self.note("syntax.unclosed_quote");
    }

    fn consume_double_quote(&mut self, w: &mut Word) {
        while let Some(c) = self.cur() {
            match c {
                '"' => {
                    self.i += 1;
                    return;
                }
                '\\' => {
                    self.i += 1;
                    match self.cur() {
                        Some(n @ ('$' | '`' | '"' | '\\')) => {
                            w.text.push(n);
                            self.i += 1;
                        }
                        Some(n) => {
                            w.text.push('\\');
                            w.text.push(n);
                            self.i += 1;
                        }
                        None => w.text.push('\\'),
                    }
                }
                '$' => self.consume_dollar(w, true),
                '`' => self.consume_backtick(w),
                _ => {
                    w.text.push(c);
                    self.i += 1;
                }
            }
        }
        self.note("syntax.unclosed_quote");
    }

    fn consume_backtick(&mut self, w: &mut Word) {
        w.expands = true;
        self.note("syntax.opaque_substitution");
        w.text.push('`');
        self.i += 1;
        while let Some(c) = self.cur() {
            w.text.push(c);
            self.i += 1;
            if c == '`' {
                return;
            }
        }
    }

    /// Cursor on `$`. Opaque spans keep their raw source text.
    fn consume_dollar(&mut self, w: &mut Word, in_dquote: bool) {
        w.text.push('$');
        self.i += 1;
        match self.cur() {
            Some('(') => {
                // $(cmd) or $((arith)) — scanned, never evaluated; the paren
                // scan counts the doubled parens fine
                w.expands = true;
                self.note("syntax.opaque_substitution");
                self.consume_paren_group(w);
            }
            Some('{') => {
                // parameter expansion; zsh flags make the content opaque
                w.expands = true;
                self.consume_brace_group(w);
            }
            Some('\'') if !in_dquote => {
                // $'ansi-c' — escapes are unprocessed, so text isn't literal
                w.quoted = true;
                w.expands = true;
                self.i += 1;
                while let Some(c) = self.cur() {
                    self.i += 1;
                    match c {
                        '\'' => return,
                        '\\' => {
                            w.text.push('\\');
                            if let Some(n) = self.cur() {
                                w.text.push(n);
                                self.i += 1;
                            }
                        }
                        _ => w.text.push(c),
                    }
                }
                self.note("syntax.unclosed_quote");
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                w.expands = true;
                while let Some(n) = self.cur() {
                    if n.is_ascii_alphanumeric() || n == '_' {
                        w.text.push(n);
                        self.i += 1;
                    } else {
                        break;
                    }
                }
            }
            Some(c)
                if c.is_ascii_digit() || matches!(c, '?' | '!' | '#' | '@' | '*' | '-' | '$') =>
            {
                w.expands = true;
                w.text.push(c);
                self.i += 1;
            }
            _ => {} // lone '$' is literal
        }
    }

    /// Cursor on `(`. Copies the group raw through its matching `)`,
    /// tracking nesting and skipping parens inside quotes. An unclosed group
    /// consumes to end of input — already maximally uncertain.
    fn consume_paren_group(&mut self, w: &mut Word) {
        let mut depth: usize = 0;
        let mut quote: Option<char> = None;
        while let Some(c) = self.cur() {
            w.text.push(c);
            self.i += 1;
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    } else if c == '\\'
                        && q == '"'
                        && let Some(n) = self.cur()
                    {
                        w.text.push(n);
                        self.i += 1;
                    }
                }
                None => match c {
                    '\'' | '"' => quote = Some(c),
                    '\\' => {
                        if let Some(n) = self.cur() {
                            w.text.push(n);
                            self.i += 1;
                        }
                    }
                    '(' => depth += 1,
                    ')' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return;
                        }
                    }
                    _ => {}
                },
            }
        }
    }

    /// Cursor on `{` (after `$`). Raw copy through the matching `}`.
    fn consume_brace_group(&mut self, w: &mut Word) {
        let mut depth: usize = 0;
        while let Some(c) = self.cur() {
            w.text.push(c);
            self.i += 1;
            match c {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return;
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(lexed: &Lexed) -> Vec<&Word> {
        lexed
            .tokens
            .iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w),
                _ => None,
            })
            .collect()
    }

    fn texts(lexed: &Lexed) -> Vec<&str> {
        words(lexed).iter().map(|w| w.text.as_str()).collect()
    }

    #[test]
    fn simple_command() {
        let l = lex("git status");
        assert_eq!(texts(&l), ["git", "status"]);
        assert!(l.uncertainty.is_empty());
        let cmds = command_words(&l);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].text, "git");
        assert!(!cmds[0].expands && !cmds[0].quoted);
    }

    #[test]
    fn control_operators_split_command_segments() {
        let l = lex("ls&&rm -rf /tmp|grep x; echo done & wait");
        let ops: Vec<&str> = l
            .tokens
            .iter()
            .filter_map(|t| match t {
                Token::Op(o) => Some(*o),
                _ => None,
            })
            .collect();
        assert_eq!(ops, ["&&", "|", ";", "&"]);
        let cmds: Vec<&str> = command_words(&l).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(cmds, ["ls", "rm", "grep", "echo", "wait"]);
    }

    #[test]
    fn quoting_resolves_where_safe() {
        let l = lex(r#"echo 'a b' "c d" e\ f '' "#);
        let ws = words(&l);
        assert_eq!(texts(&l), ["echo", "a b", "c d", "e f", ""]);
        assert!(!ws[0].quoted);
        assert!(ws[1].quoted && ws[2].quoted && ws[3].quoted && ws[4].quoted);
        assert!(l.uncertainty.is_empty());
    }

    #[test]
    fn quoted_command_word_still_found() {
        let l = lex(r#"'git' status"#);
        let cmds = command_words(&l);
        assert_eq!(cmds[0].text, "git");
        assert!(cmds[0].quoted);
    }

    #[test]
    fn unclosed_quote_is_flagged() {
        let l = lex("echo 'oops");
        assert!(l.uncertainty.contains(&"syntax.unclosed_quote"));
        assert_eq!(texts(&l), ["echo", "oops"]);
        let l = lex("echo \"oops");
        assert!(l.uncertainty.contains(&"syntax.unclosed_quote"));
    }

    #[test]
    fn command_substitution_is_opaque_and_raw() {
        let l = lex("echo $(rm -rf /) tail");
        assert!(l.uncertainty.contains(&"syntax.opaque_substitution"));
        assert_eq!(texts(&l), ["echo", "$(rm -rf /)", "tail"]);
        let ws = words(&l);
        assert!(ws[1].expands);
        // the substitution's content never becomes a command word
        let cmds: Vec<&str> = command_words(&l).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(cmds, ["echo"]);
    }

    #[test]
    fn substitution_inside_double_quotes_is_flagged() {
        let l = lex(r#"echo "today: $(date)""#);
        assert!(l.uncertainty.contains(&"syntax.opaque_substitution"));
        assert!(words(&l)[1].expands);
    }

    #[test]
    fn backtick_arithmetic_process_substitution() {
        let l = lex("echo `date`");
        assert!(l.uncertainty.contains(&"syntax.opaque_substitution"));

        let l = lex("echo $((1+2))");
        assert!(l.uncertainty.contains(&"syntax.opaque_substitution"));
        assert_eq!(texts(&l)[1], "$((1+2))");

        let l = lex("diff <(sort a) <(sort b)");
        assert!(l.uncertainty.contains(&"syntax.opaque_substitution"));
        assert_eq!(texts(&l), ["diff", "<(sort a)", "<(sort b)"]);
        let cmds: Vec<&str> = command_words(&l).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(cmds, ["diff"]);
    }

    #[test]
    fn parameter_expansion_expands_without_opaque_note() {
        let l = lex("echo $HOME ${PATH:-x} $1 $?");
        let ws = words(&l);
        assert!(ws[1].expands && ws[2].expands && ws[3].expands && ws[4].expands);
        assert_eq!(texts(&l), ["echo", "$HOME", "${PATH:-x}", "$1", "$?"]);
        assert!(!l.uncertainty.contains(&"syntax.opaque_substitution"));
    }

    #[test]
    fn redirects_and_targets_are_not_command_words() {
        let l = lex("make >build.log 2>&1");
        let redirects: Vec<&str> = l
            .tokens
            .iter()
            .filter_map(|t| match t {
                Token::Redirect(r) => Some(r.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(redirects, [">", "2>&"]);
        let cmds: Vec<&str> = command_words(&l).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(cmds, ["make"]);

        let l = lex("sort <<< 'b a'");
        assert_eq!(
            command_words(&l)
                .iter()
                .map(|w| w.text.as_str())
                .collect::<Vec<_>>(),
            ["sort"]
        );
        assert!(l.uncertainty.is_empty(), "herestring is not a heredoc");
    }

    #[test]
    fn heredoc_is_flagged() {
        let l = lex("cat <<EOF");
        assert!(l.uncertainty.contains(&"syntax.heredoc"));
        let cmds: Vec<&str> = command_words(&l).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(cmds, ["cat"], "heredoc delimiter is not a command word");
    }

    #[test]
    fn mid_word_redirect_splits() {
        let l = lex("echo hi>out.txt");
        assert_eq!(texts(&l), ["echo", "hi", "out.txt"]);
        assert!(matches!(&l.tokens[2], Token::Redirect(r) if r == ">"));
    }

    #[test]
    fn assignments_are_skipped_in_command_position() {
        let l = lex("FOO=bar BAZ+=x env | cat");
        let cmds: Vec<&str> = command_words(&l).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(cmds, ["env", "cat"]);
        // assignment alone: no command word at all
        assert!(command_words(&lex("FOO=bar")).is_empty());
        // '=' later in a non-assignment word doesn't hide a command word
        let l = lex("./run --opt=1");
        assert_eq!(command_words(&l)[0].text, "./run");
    }

    #[test]
    fn globs_mark_expansion() {
        let l = lex("rm *.log file?.txt [ab] ~/x");
        for w in &words(&l)[1..] {
            assert!(w.expands, "glob word not marked: {w:?}");
        }
        let l = lex("ls foo(.)"); // zsh glob qualifier
        assert!(l.uncertainty.contains(&"syntax.zsh_glob"));
    }

    #[test]
    fn subshell_parens_are_operators() {
        let l = lex("(cd /tmp && make)");
        let cmds: Vec<&str> = command_words(&l).iter().map(|w| w.text.as_str()).collect();
        assert_eq!(cmds, ["cd", "make"]);
    }

    #[test]
    fn ansi_c_quotes_are_non_literal() {
        let l = lex(r"echo $'a\nb'");
        let ws = words(&l);
        assert!(ws[1].quoted && ws[1].expands);
    }

    #[test]
    fn fuzz_smoke_never_panics_and_stays_bounded() {
        // Deterministic xorshift byte soup over shell metacharacters; the
        // assertions double as an infinite-loop guard (tokens can't exceed
        // input length because every token consumes at least one char).
        let alphabet: Vec<char> = "ab c'\"\\$(){}[]|&;<>`*?~^#=%!-\n\t0129注€日"
            .chars()
            .collect();
        let mut state: u64 = 0x243F_6A88_85A3_08D3;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 72) as usize;
            let s: String = (0..len)
                .map(|_| alphabet[(next() as usize) % alphabet.len()])
                .collect();
            let lexed = lex(&s);
            let _ = command_words(&lexed);
            assert!(lexed.tokens.len() <= s.chars().count());
            assert!(lexed.uncertainty.len() <= 4);
        }
    }
}
