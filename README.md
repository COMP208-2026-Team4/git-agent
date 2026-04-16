# git-agent

manages git permissions over ssh, and provides REST API for git-related activities in the web UI.

## building

git-agent uses nix to avoid dependancy hell. ensure nix is installed on your system (<https://nixos.org/download/>), and `nix-command` and `flakes` experimental features are enabled.

then run

```sh
nix develop
```

### nix-less build 😢

ensure the following dependencies are installed and available in `$PATH`

```
mold
cargo
```

then build

```
cargo build --release
```
