//! The shell rc text both images share, in one place.
//!
//! A workspace writes these from its pod prelude (`workspace::prelude`, as root into `/etc`); the
//! bench image bakes the same bytes in at build time (`deploy/bench/zshrc`,
//! `deploy/bench/starship.toml`). Two copies of a prompt drift silently — one image grows a
//! completion cache or a starship field and the other does not, and nobody notices until a person
//! says their bench shell "looks wrong" — so the text lives here and
//! `tests::bench_rc_files_match` fails the build when the rendered files stop matching.
//!
//! The prelude writes them with `printf`, which is why `printf_lines`/`printf_text` are here too:
//! the constants are the FILE content, the helpers are the one place that knows how to quote it.
//! Neither quotes a `'` — nothing here contains one, and `printf_lines` asserts it in debug —
//! because a single quote inside a single-quoted word would need the printf quoting nested a
//! second time, which is exactly the trap this module exists to avoid repeating.

/// `/etc/zshrc` (workspace) / `/etc/zsh/zshrc` (bench): the platform's half of an interactive zsh.
/// The person's own `~/.config/zsh/.zshrc` (see `seed_zshrc`) runs after and wins.
pub const ZSHRC: &str = "\
[[ -o interactive ]] || return 0
[ \"$PWD\" = \"$HOME\" ] && [ -d \"$KL_WORKSPACE\" ] && cd \"$KL_WORKSPACE\"
[ -e \"$HOME/.config/starship.toml\" ] || export STARSHIP_CONFIG=/etc/starship.toml
mkdir -p \"${XDG_CACHE_HOME:-$HOME/.cache}/zsh\"
autoload -Uz compinit && compinit -d \"${XDG_CACHE_HOME:-$HOME/.cache}/zsh/zcompdump\"
zstyle \":completion:*\" menu select
[ -r /etc/profile.d/kl-build.sh ] && sh /etc/profile.d/kl-build.sh
";

/// `/etc/starship.toml`: the fallback prompt, used only while the person keeps none of their own.
pub const STARSHIP_TOML: &str = "format = \"$directory$git_branch$git_status$cmd_duration$line_break$character\"\n";

/// The person's own `~/.config/zsh/.zshrc`, seeded once and theirs to edit afterwards.
pub fn seed_zshrc(path_env: &str) -> String {
    format!(
        "export PATH={path_env}\n\
         eval \"$(dircolors -b)\"\n\
         zstyle \":completion:*\" list-colors \"${{(s.:.)LS_COLORS}}\"\n\
         alias ls=\"ls --color=auto\" grep=\"grep --color=auto\"\n\
         eval \"$(starship init zsh)\"\n"
    )
}

/// `printf '%s\n' 'line' …` — one shell word per line, so nothing in the content is ever
/// interpreted. The caller appends the redirect.
pub(super) fn printf_lines(content: &str) -> String {
    let words: Vec<String> = content.lines().map(|l| format!("'{l}'")).collect();
    debug_assert!(!content.contains('\''), "a single quote needs the quoting nested twice");
    format!("printf '%s\\n' {}", words.join(" "))
}

/// `printf 'text\n…'` — the whole content as one single-quoted format string, for a seed that is
/// written only when absent (the `[ -e … ] ||` in front of it must guard one command, not seven).
pub(super) fn printf_text(content: &str) -> String {
    debug_assert!(!content.contains('\'') && !content.contains('%'), "unquotable in a printf format");
    format!("printf '{}'", content.replace('\n', "\\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bench image copies these files in; the workspace renders the constants at pod start.
    /// Equality here is the only thing keeping the two prompts the same prompt.
    #[test]
    fn bench_rc_files_match() {
        assert_eq!(include_str!("../../../../deploy/bench/zshrc"), ZSHRC, "run: the constant is the source, the file is the copy");
        assert_eq!(include_str!("../../../../deploy/bench/starship.toml"), STARSHIP_TOML);
    }

    #[test]
    fn printf_helpers_round_trip_through_a_real_shell() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rc");
        for (cmd, want) in [(printf_lines(ZSHRC), ZSHRC.to_string()), (printf_text(&seed_zshrc("/bin")), seed_zshrc("/bin"))] {
            let script = format!("{cmd} > {}", out.display());
            assert!(std::process::Command::new("sh").arg("-c").arg(&script).status().unwrap().success(), "{script}");
            assert_eq!(std::fs::read_to_string(&out).unwrap(), want);
        }
    }
}
