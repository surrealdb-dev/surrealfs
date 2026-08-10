//! Rendering a [`Confinement`] as a macOS Seatbelt (SBPL) profile.
//!
//! SBPL is a Scheme-shaped policy language read by `sandbox-exec`. It is formally deprecated and
//! has been for years, while remaining the mechanism macOS actually enforces and the one every
//! comparable tool uses; the deprecation means the interface may move, not that the confinement
//! is weak.
//!
//! The whole file is a pure function from policy to text, which is what makes it testable without
//! spawning anything — including the part that matters most, which is that a path can never break
//! out of the string literal it is written into.

use std::path::Path;

use crate::Confinement;

/// Escape a path for inclusion in an SBPL string literal.
///
/// This is the security boundary of this module. A path is attacker-influenced whenever an agent
/// chooses its own working directory, and SBPL string literals honour backslash escapes, so a
/// path containing `"` would otherwise close the literal and let the remainder be read as policy
/// — appending `(allow default)` to the very profile meant to contain it.
///
/// Order matters: backslashes are escaped first, or the backslash introduced when escaping a
/// quote would itself be escaped and the quote left bare.
fn escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Render a policy as a complete SBPL profile.
///
/// The profile denies everything first and then adds back only what the policy names. Read rules
/// are emitted for writable paths as well as readable ones, matching `allow_write`'s promise that
/// writable implies readable.
pub fn seatbelt_profile(confinement: &Confinement) -> String {
    let mut out = String::from("(version 1)\n(deny default)\n");

    // Without these, nothing runs at all: a process cannot be exec'd, cannot resolve a dynamic
    // library, and cannot ask the kernel its own page size. They grant no filesystem reach.
    out.push_str("(allow process-exec)\n");
    out.push_str("(allow process-fork)\n");
    out.push_str("(allow sysctl-read)\n");
    out.push_str("(allow mach-lookup)\n");
    out.push_str("(allow signal (target self))\n");

    // The root directory itself. Determined empirically: no combination of `subpath` grants —
    // not even the union of every top-level directory — lets a process start, because none of
    // them covers `/`. `file-read-metadata` on `/` is *not* sufficient; dyld wants a real read.
    out.push_str("(allow file-read* (literal \"/\"))\n");

    // Ancestor traversal. Resolving `/a/b/c` stats each component, so a grant on the leaf is
    // useless without the path to it. This is metadata only: it lets a confined process learn
    // that a path exists and nothing about its contents. That is a real if narrow disclosure —
    // an agent can enumerate directory structure it cannot read — and it is accepted knowingly
    // rather than overlooked, because the alternative is computing the ancestor set of every
    // grant and getting it subtly wrong.
    out.push_str("(allow file-read-metadata (subpath \"/\"))\n");

    // A process with no writable temp directory fails in ways that look like bugs in the agent
    // rather than the policy, and /dev/null is assumed by essentially every runtime.
    out.push_str("(allow file-read* file-write* (literal \"/dev/null\"))\n");
    out.push_str("(allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))\n");

    for path in confinement.readable().iter().chain(confinement.writable()) {
        out.push_str(&format!(
            "(allow file-read* (subpath \"{}\"))\n",
            escape(path)
        ));
    }
    for path in confinement.writable() {
        out.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            escape(path)
        ));
    }

    if confinement.network_allowed() {
        out.push_str("(allow network-outbound)\n");
        out.push_str("(allow network-bind)\n");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_policy_denies_by_default_and_grants_no_paths() {
        let profile = seatbelt_profile(&Confinement::closed());
        assert!(profile.starts_with("(version 1)\n(deny default)\n"));
        assert!(
            !profile.contains("(allow file-read* (subpath"),
            "a closed policy granted a subpath: {profile}"
        );
        assert!(!profile.contains("file-write* (subpath"));
        assert!(!profile.contains("network-outbound"));
    }

    #[test]
    fn writable_paths_are_also_readable() {
        let profile = seatbelt_profile(&Confinement::closed().allow_write("/mnt/work"));
        assert!(
            profile.contains(r#"(allow file-read* (subpath "/mnt/work"))"#),
            "a writable path must also be readable: {profile}"
        );
        assert!(profile.contains(r#"(allow file-write* (subpath "/mnt/work"))"#));
    }

    #[test]
    fn a_readable_path_does_not_become_writable() {
        let profile = seatbelt_profile(&Confinement::closed().allow_read("/usr/lib"));
        assert!(profile.contains(r#"(allow file-read* (subpath "/usr/lib"))"#));
        assert!(
            !profile.contains(r#"(allow file-write* (subpath "/usr/lib"))"#),
            "a read grant leaked write access: {profile}"
        );
    }

    #[test]
    fn network_is_off_unless_asked_for() {
        assert!(!seatbelt_profile(&Confinement::closed()).contains("network-outbound"));
        let allowed = seatbelt_profile(&Confinement::closed().allow_network());
        assert!(allowed.contains("(allow network-outbound)"));
    }

    /// The security property of this module: a path can never escape its string literal.
    #[test]
    fn a_path_cannot_break_out_of_its_string_literal() {
        // The attack: end the literal, then append a rule that grants everything.
        let hostile = Confinement::closed().allow_read(r#"/tmp/x")) (allow default) (subpath "/"#);
        let profile = seatbelt_profile(&hostile);

        // The injected text survives as escaped *content* inside the literal, which is harmless.
        // What must not happen is it becoming a rule — and rules are the lines of this profile,
        // so the property is that no line is one, not that the bytes are absent anywhere.
        assert!(
            !profile.lines().any(|line| line.trim() == "(allow default)"),
            "a crafted path injected policy: {profile}"
        );
        // The quotes survive as escaped content rather than as syntax.
        assert!(
            profile.contains(r#"\"))"#),
            "the quote was not escaped: {profile}"
        );
        // Exactly the rules we intended, and no more.
        assert_eq!(
            profile.matches("(allow file-read* (subpath").count(),
            1,
            "one read rule was asked for: {profile}"
        );
    }

    /// Backslashes must be escaped before quotes, or escaping a quote produces a backslash that
    /// is then escaped and leaves the quote bare.
    #[test]
    fn backslashes_are_escaped_before_quotes() {
        let profile = seatbelt_profile(&Confinement::closed().allow_read(r#"/tmp/a\"b"#));
        assert!(
            profile.contains(r#"/tmp/a\\\"b"#),
            "backslash-then-quote was mis-escaped: {profile}"
        );
        assert!(!profile.lines().any(|l| l.trim() == "(allow default)"));
    }

    #[test]
    fn the_standard_mount_policy_confines_writes_to_the_mount() {
        let profile = seatbelt_profile(&Confinement::for_mount("/mnt/agent"));
        assert!(profile.contains(r#"(allow file-write* (subpath "/mnt/agent"))"#));
        assert_eq!(
            profile.matches("file-write* (subpath").count(),
            1,
            "the mount must be the only writable subpath: {profile}"
        );
        // Host configuration and credentials are not readable.
        assert!(!profile.contains(r#""/etc""#), "{profile}");
        assert!(!profile.contains(r#""/Users""#), "{profile}");
    }
}
