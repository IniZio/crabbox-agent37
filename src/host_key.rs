//! Shell script that prints a VM's ed25519 host public key.
//!
//! agent37 images run sshd from `/.agent37` with its own config, so the key lives
//! wherever that config's `HostKey` line says. Stock `/etc/ssh` is the fallback.
//! Without a `.pub` file and without `ssh-keygen`, the public blob is cut out of the
//! unencrypted private key on the VM; nothing else of the private key is printed.

/// Script template; `@ROOT@` is the path prefix, empty on a real VM.
const TEMPLATE: &str = r#"root='@ROOT@'
PATH=/.agent37/bin:$PATH
cfg=$(awk '$1=="HostKey" && $2 ~ /ed25519/ {print $2; exit}' "$root/.agent37/etc/sshd_config" 2>/dev/null)
[ -n "$cfg" ] && cfg="$root$cfg"
for k in "$cfg" "$root/.agent37/etc/ssh_host_ed25519_key" "$root/etc/ssh/ssh_host_ed25519_key"; do
  [ -n "$k" ] || continue
  if [ -r "$k.pub" ]; then
    out=$(cat "$k.pub")
  elif [ -r "$k" ] && command -v ssh-keygen >/dev/null 2>&1; then
    out=$(ssh-keygen -y -f "$k" 2>/dev/null)
  elif [ -r "$k" ]; then
    b=$({ sed '1d;$d' "$k" | tr -d '\n' | base64 -d | tail -c +44 | head -c 51 | base64 | tr -d '\n'; } 2>/dev/null)
    case $b in AAAAC3NzaC1lZDI1NTE5*) out="ssh-ed25519 $b" ;; *) out= ;; esac
  else
    continue
  fi
  [ -n "$out" ] || continue
  printf '%s\n' "$out" | awk 'NR==1 {print $1" "$2}'
  exit 0
done
echo 'no ssh-ed25519 host key found' >&2
exit 1
"#;

/// Script that prints `type base64` of the host key found under `root`.
pub(crate) fn command(root: &str) -> String {
    TEMPLATE.replace("@ROOT@", root)
}

/// Script for a real VM, where `root` is empty.
pub(crate) fn production() -> String {
    command("")
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output};

    use super::command;

    /// Test result type.
    type Res = Result<(), Box<dyn Error>>;

    /// Generates an ed25519 key at `path`; returns the first two fields of its `.pub`.
    fn keygen(path: &Path) -> Result<String, Box<dyn Error>> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(path)
            .status()?;
        assert!(status.success());
        let public = fs::read_to_string(path.with_extension("pub"))
            .or_else(|_| fs::read_to_string(format!("{}.pub", path.display())))?;
        let mut fields = public.split_whitespace();
        Ok(format!(
            "{} {}",
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default()
        ))
    }

    /// Runs the script against `root`.
    fn run(root: &Path) -> Result<Output, Box<dyn Error>> {
        Ok(Command::new("sh")
            .arg("-c")
            .arg(command(&root.display().to_string()))
            .output()?)
    }

    /// Writes an sshd config naming the agent37 key.
    fn write_config(root: &Path) -> Result<(), Box<dyn Error>> {
        fs::write(
            root.join(".agent37/etc/sshd_config"),
            "Port 22\nHostKey /.agent37/etc/ssh_host_ed25519_key\n",
        )?;
        Ok(())
    }

    #[test]
    fn prints_configured_pub_without_comment() -> Res {
        let dir = tempfile::tempdir()?;
        let expected = keygen(&dir.path().join(".agent37/etc/ssh_host_ed25519_key"))?;
        write_config(dir.path())?;
        let out = run(dir.path())?;
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout)?, format!("{expected}\n"));
        Ok(())
    }

    /// Runs the script against `root` with a `PATH` that has no `ssh-keygen`.
    fn run_without_keygen(root: &Path) -> Result<Output, Box<dyn Error>> {
        let bin = root.join("tools");
        fs::create_dir_all(&bin)?;
        for tool in [
            "sed", "tr", "base64", "tail", "head", "awk", "cat", "dirname",
        ] {
            if let Some(src) = which(tool) {
                std::os::unix::fs::symlink(src, bin.join(tool))?;
            }
        }
        Ok(Command::new("/bin/sh")
            .arg("-c")
            .arg(command(&root.display().to_string()))
            .env("PATH", &bin)
            .output()?)
    }

    /// Finds `tool` on the host `PATH`.
    fn which(tool: &str) -> Option<std::path::PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join(tool))
            .find(|p| p.is_file())
    }

    #[test]
    fn derives_key_with_ssh_keygen_when_pub_is_missing() -> Res {
        let dir = tempfile::tempdir()?;
        let key = dir.path().join(".agent37/etc/ssh_host_ed25519_key");
        let expected = keygen(&key)?;
        write_config(dir.path())?;
        fs::remove_file(format!("{}.pub", key.display()))?;
        let out = run(dir.path())?;
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout)?, format!("{expected}\n"));
        Ok(())
    }

    #[test]
    fn extracts_key_from_private_file_without_ssh_keygen() -> Res {
        let dir = tempfile::tempdir()?;
        let key = dir.path().join(".agent37/etc/ssh_host_ed25519_key");
        let expected = keygen(&key)?;
        write_config(dir.path())?;
        fs::remove_file(format!("{}.pub", key.display()))?;
        let out = run_without_keygen(dir.path())?;
        assert_eq!(String::from_utf8(out.stdout)?, format!("{expected}\n"));
        assert!(out.status.success());
        Ok(())
    }

    #[test]
    fn rejects_garbage_private_key_without_ssh_keygen() -> Res {
        let dir = tempfile::tempdir()?;
        fs::create_dir_all(dir.path().join(".agent37/etc"))?;
        fs::write(
            dir.path().join(".agent37/etc/ssh_host_ed25519_key"),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nbm90IGEga2V5IGF0IGFsbCBub3Igbm90IGVkMjU1MTkgYXQgYWxs\n-----END OPENSSH PRIVATE KEY-----\n",
        )?;
        write_config(dir.path())?;
        let out = run_without_keygen(dir.path())?;
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        Ok(())
    }

    #[test]
    fn falls_back_to_etc_ssh() -> Res {
        let dir = tempfile::tempdir()?;
        let expected = keygen(&dir.path().join("etc/ssh/ssh_host_ed25519_key"))?;
        let out = run(dir.path())?;
        assert!(out.status.success());
        assert_eq!(String::from_utf8(out.stdout)?, format!("{expected}\n"));
        Ok(())
    }

    #[test]
    fn fails_when_no_key_exists() -> Res {
        let dir = tempfile::tempdir()?;
        let out = run(dir.path())?;
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(
            String::from_utf8(out.stderr)?,
            "no ssh-ed25519 host key found\n"
        );
        Ok(())
    }
}
