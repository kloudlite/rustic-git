//! The shell rc text both images share, in one place.
//!
//! A workspace writes these from its pod prelude (`workspace::prelude`, as root into `/etc`), and
//! that is now the only reader: the bench image used to bake the same bytes in at build time, but
//! a bench shell is the WORKSPACE container's shell (`/pty` splices to the tool server over
//! 127.0.0.1), so there is one prompt again and nothing left to hold equal.
//!
//! The prelude writes them with `printf`, which is why `printf_lines`/`printf_text` are here too:
//! the constants are the FILE content, the helpers are the one place that knows how to quote it.
//! Neither quotes a `'` — nothing here contains one, and `printf_lines` asserts it in debug —
//! because a single quote inside a single-quoted word would need the printf quoting nested a
//! second time, which is exactly the trap this module exists to avoid repeating.

/// `/etc/zshrc`: the platform's half of an interactive zsh in a workspace pod.
/// The person's own `~/.config/zsh/.zshrc` (see `seed_zshrc`) runs after and wins.
///
/// zsh saves NO history without `SAVEHIST` — the default is 0, so every shell in a workspace
/// started blank however long the person had been in it. `HISTFILE` itself is pod env
/// (`k8s/workspace.rs`), on the per-node local state dir; written incrementally and shared so a
/// second exec or a pod kill does not lose what was typed in the first.
/// `_kl_rehash`: zsh caches command lookups, so a package `kl pkg add` just installed is
/// "command not found" in every shell that was already open until `rehash` — and a named terminal
/// outlives every socket, so it is always one that was already open. One `readlink` per prompt catches the profile symlink
/// moving and rehashes only then (owner, 2026-09-17 04:12 IST: "installed package is not accessible").
pub const ZSHRC: &str = "\
[[ -o interactive ]] || return 0
[ \"$PWD\" = \"$HOME\" ] && [ -d \"$KL_WORKSPACE\" ] && cd \"$KL_WORKSPACE\"
[ -e \"$HOME/.config/starship.toml\" ] || export STARSHIP_CONFIG=/etc/starship.toml
mkdir -p \"${XDG_CACHE_HOME:-$HOME/.cache}/zsh\"
autoload -Uz compinit && compinit -d \"${XDG_CACHE_HOME:-$HOME/.cache}/zsh/zcompdump\"
zstyle \":completion:*\" menu select
HISTSIZE=50000
SAVEHIST=50000
setopt appendhistory incappendhistory sharehistory histignorealldups histignorespace
_kl_profile=$(readlink /nix/profile/current 2>/dev/null)
_kl_rehash() { local p=$(readlink /nix/profile/current 2>/dev/null); [[ $p != $_kl_profile ]] && { _kl_profile=$p; rehash; }; }
precmd_functions+=(_kl_rehash)
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
