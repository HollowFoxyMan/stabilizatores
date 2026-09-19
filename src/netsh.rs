//! Shared `netsh` invocation helper.
//!
//! Both the DNS feature (`netsh interface ip ...`, `netsh dns ...`) and the
//! MTU feature (`netsh interface ipv4 set subinterface ...`) shell out to
//! `netsh`. Running one process per command keeps the code honest: no shell
//! quoting ambiguity, localized output is never parsed, and every command is
//! best-effort with a clear error string.

use std::io;

/// Runs `netsh` with the given argument vector. Returns the captured stdout
/// on success; on failure the stderr is folded into a single-line error so
/// messages stay readable in the menu.
pub fn run(args: &[&str]) -> Result<String, io::Error> {
    let output = std::process::Command::new("netsh").args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(io::Error::other(if stderr.is_empty() {
            "netsh exited with an error".to_string()
        } else {
            format!("netsh: {stderr}")
        }))
    }
}

fn quoted(tag: &str, value: &str) -> Result<String, String> {
    if value.is_empty() {
        return Err(format!("empty {tag} value"));
    }
    if value.contains('"') {
        return Err(format!("{tag} value contains a quote: {value}"));
    }
    Ok(format!("{tag}=\"{value}\""))
}

/// Builds the `name="..."` argument netsh expects for an interface name.
pub fn name_arg(friendly: &str) -> Result<String, String> {
    quoted("name", friendly)
}

/// Builds the `interface="..."` argument used by the `netsh dns` context.
pub fn interface_arg(friendly: &str) -> Result<String, String> {
    quoted("interface", friendly)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_quotes_always() {
        assert_eq!(name_arg("Wi-Fi").unwrap(), "name=\"Wi-Fi\"");
        assert_eq!(name_arg("Ethernet 2").unwrap(), "name=\"Ethernet 2\"");
    }

    #[test]
    fn interface_arg_uses_interface_tag() {
        assert_eq!(interface_arg("Wi-Fi").unwrap(), "interface=\"Wi-Fi\"");
    }

    #[test]
    fn quoted_rejects_quotes() {
        assert!(name_arg("bad\"name").is_err());
    }

    #[test]
    fn quoted_rejects_empty() {
        assert!(name_arg("").is_err());
    }
}
