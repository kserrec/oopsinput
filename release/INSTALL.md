# Install oopsinput @VERSION@

This archive is the x86_64 Linux release of oopsinput. It needs an interactive
Zsh shell, but it does not need Rust, Git, root access, or a source checkout.
The installer makes no network connection and starts no background process.

Run the installer from this extracted directory:

```sh
zsh install.zsh
```

A fresh install requires you to choose one starting mode. Nothing is selected
for you:

- Shadow never interrupts; it only analyzes and records locally.
- Suggest also asks about likely misspelled command names.
- Warn also shows danger prompts; no answer eventually runs the original.
- Confirm makes the highest-risk prompts require a choice; no answer cancels.

The installer shows every file it will change before committing the install.
It installs under `~/.local`, creates a user-only config under `~/.config` when
needed, preserves the original `~/.zshrc` bytes in
`~/.zshrc.oopsinput-backup`, and adds one marked block to `~/.zshrc`. It does
not source that file or change `PATH`.

After installation, open a new terminal and verify the live shell:

```sh
"$HOME/.local/bin/oopsinput" doctor
```

The install is ready only when that command prints `result: ready`.

For deliberate promptless automation of a fresh install, pass an explicit
mode, for example:

```sh
zsh install.zsh --mode shadow
```

Rerunning `zsh install.zsh` updates a healthy installation and preserves its
existing config byte-for-byte. Supplying `--mode` during an update is rejected;
edit the config directly when you intend to change modes.

To remove the runtime later, without retaining this directory:

```sh
zsh "$HOME/.local/share/oopsinput/uninstall.zsh"
```

The uninstaller keeps the config, recorded state, and original shell backup.
To delete recorded state, run `"$HOME/.local/bin/oopsinput" purge` before the
uninstaller.
