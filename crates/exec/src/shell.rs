//! The shell `run_command` hands its command to: `sh -c` on Unix and in containers; on
//! Windows, PowerShell (`pwsh` when installed, else the Windows PowerShell every Windows has).

use std::ffi::{OsStr, OsString};

use base64::Engine;

/// Which shell runs a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `sh -c` (POSIX).
    Sh,
    /// PowerShell.
    PowerShell,
}

impl Shell {
    /// The shell for commands on this computer.
    pub fn host() -> Self {
        if cfg!(windows) { Self::PowerShell } else { Self::Sh }
    }

    /// How the tool description names it.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Sh => "`sh -c`",
            Self::PowerShell => "PowerShell",
        }
    }

    /// Program and arguments that run `command`.
    pub fn argv(self, command: &str) -> (OsString, Vec<OsString>) {
        match self {
            Self::Sh => ("sh".into(), vec!["-c".into(), command.into()]),
            Self::PowerShell => {
                let program = if byob_sandbox::which(OsStr::new("pwsh")).is_some() { "pwsh" } else { "powershell" };
                let args = ["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"];
                let mut v: Vec<OsString> = args.iter().map(OsString::from).collect();
                v.push(encode(&powershell_script(command)).into());
                (program.into(), v)
            }
        }
    }
}

/// `command` with UTF-8 output (Windows PowerShell writes the console code page into pipes
/// otherwise) and UTF-8 files (its `Set-Content` and `Out-File` write the ANSI code page or
/// UTF-16, so a `✓` became `?`; 5.1's UTF-8 has a BOM, PowerShell 7's doesn't), no progress
/// bars, and a failing program's exit code as the script's.
fn powershell_script(command: &str) -> String {
    format!(
        "$ProgressPreference = 'SilentlyContinue'\n\
         [Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n\
         $OutputEncoding = [System.Text.Encoding]::UTF8\n\
         if ($PSVersionTable.PSVersion.Major -lt 6) {{ $PSDefaultParameterValues['*:Encoding'] = 'utf8' }}\n\
         {command}\n\
         if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}\n"
    )
}

/// `-EncodedCommand` takes base64 of UTF-16LE. Passing the script that way avoids Windows
/// PowerShell's quote stripping in `-Command` arguments.
fn encode(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sh_takes_the_command_as_is() {
        let (p, a) = Shell::Sh.argv("echo 'hi' | wc -c");
        assert_eq!((p, a), ("sh".into(), vec![OsString::from("-c"), "echo 'hi' | wc -c".into()]));
    }

    #[test]
    fn powershell_gets_the_command_encoded_with_quotes_intact() {
        let (p, a) = Shell::PowerShell.argv(r#"Get-ChildItem "my files" | Select-Object -First 2"#);
        assert!(p == "pwsh" || p == "powershell", "{p:?}");
        assert_eq!(a.get(a.len() - 2).map(OsString::as_os_str), Some(OsStr::new("-EncodedCommand")));
        let b64 = a.last().and_then(|s| s.to_str()).unwrap();
        let raw = base64::engine::general_purpose::STANDARD.decode(b64).unwrap();
        let units: Vec<u16> = raw.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let script = String::from_utf16(&units).unwrap();
        assert!(script.contains(r#"Get-ChildItem "my files" | Select-Object -First 2"#), "{script}");
        assert!(script.contains("$PSDefaultParameterValues['*:Encoding'] = 'utf8'"), "{script}");
        assert!(script.contains("[Console]::OutputEncoding") && script.trim_end().ends_with("exit $LASTEXITCODE }"), "{script}");
    }

    #[test]
    fn the_host_shell_matches_the_os() {
        assert_eq!(Shell::host(), if cfg!(windows) { Shell::PowerShell } else { Shell::Sh });
    }
}
