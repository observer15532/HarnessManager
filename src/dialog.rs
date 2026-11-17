//! Two one-off dialogs, borrowed from the system instead of drawn by hand.
//!
//! Windows has no built-in "ask for a string" control a Rust binary can call in
//! one line, but PowerShell 5.1 ships both a text input box and a folder
//! picker. Each dialog is a short script handed to `powershell.exe` with
//! `-EncodedCommand`, so nothing is written to disk and the execution policy
//! never comes into play.

use crate::dsh;

/// Base64 of the UTF-16LE script body, which is what `-EncodedCommand` expects.
fn encode(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|unit| unit.to_le_bytes()).collect();

    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[(triple >> 18) as usize & 63] as char);
        out.push(TABLE[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn run(script: &str) -> Option<String> {
    let encoded = encode(script);
    let (ok, stdout, stderr) = match dsh::run_exe(
        "powershell.exe",
        &[
            "-NoProfile",
            "-STA",
            "-WindowStyle",
            "Hidden",
            "-EncodedCommand",
            &encoded,
        ],
    ) {
        Ok(result) => result,
        Err(error) => {
            crate::log_event(&format!("dialog: 无法启动 powershell.exe: {error}"));
            return None;
        }
    };

    if !ok {
        crate::log_event(&format!("dialog: powershell 退出异常: {}", stderr.trim()));
        return None;
    }
    if !stderr.trim().is_empty() {
        crate::log_event(&format!("dialog: {}", stderr.trim()));
    }

    let value = stdout.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn quote(text: &str) -> String {
    text.replace('\'', "''")
}

pub fn input_text(title: &str, prompt: &str, default: &str) -> Option<String> {
    let script = format!(
        "Add-Type -AssemblyName Microsoft.VisualBasic\n\
         $value = [Microsoft.VisualBasic.Interaction]::InputBox('{prompt}', '{title}', '{default}')\n\
         if ($value) {{ [Console]::Out.WriteLine($value.Trim()) }}\n",
        prompt = quote(prompt),
        title = quote(title),
        default = quote(default),
    );
    run(&script)
}

/// Kept for features that do need a folder; the profile flow no longer asks for
/// one, because dsh boots anywhere and the Web UI owns workspace selection.
#[allow(dead_code)]
pub fn pick_folder(title: &str, description: &str) -> Option<String> {
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms\n\
         $dialog = New-Object System.Windows.Forms.FolderBrowserDialog\n\
         $dialog.Description = '{description}'\n\
         $dialog.ShowNewFolderButton = $false\n\
         $dialog.UseDescriptionForTitle = $true\n\
         if ($dialog.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) {{\n\
         \x20 [Console]::Out.WriteLine($dialog.SelectedPath)\n\
         }}\n",
        description = quote(description),
    );
    let _ = title;
    run(&script)
}

#[cfg(test)]
mod tests {
    use super::encode;

    /// `-EncodedCommand` wants base64 of UTF-16LE; a bad pad here fails silently.
    #[test]
    fn encodes_utf16le() {
        assert_eq!(encode("hi"), "aABpAA==");
    }
}
