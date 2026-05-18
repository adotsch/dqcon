# DQCON

![dqcon demo](dqcon_demo.gif)

`dqcon` is a modern remake of `qcon`, a powerful, cross-platform terminal client for **kdb+/q**. It provides a modern TUI (Terminal User Interface) experience with features designed for high productivity, such as connection list, persistent session history, multi-line editing, and command repetition.

## Features

Current features:

- **Modern TUI**: Built with Rust, Ratatui, and Crossterm for a responsive and visually clear interface.
- **Cross-Platform**: Native binaries for Linux, Windows, and macOS (Intel and ARM).
- **Async interaction**: You don't need to wait for a command to finish to get a prompt again.
- **Multi-line Editing**: Support for complex queries with newline insertion (`Ctrl+N`).
- **Text selection/clipboard**: Select and copy/paste text using keyboard or mouse.
- **Persistent sessions**: Preserve command history and last results across sessions using `-m` mode.
- **Command repetition**: Periodically execute commands (e.g., monitoring a table) with configurable delays (`Ctrl+R`).
- **Simple Mode**: A basic REPL mode (`-s`) for use in environments without full TUI support.

Planned features:

- Code completion
- Syntax highlighting of queries
- Tree view of namespaces and dictionaries
- Function loading and editing

## Usage

Run the executable with the connection string as an argument

```sh
dqcon [-m] [host]:port[:user[:pass]]
```
or just run 
```sh
dqcon [-m]
```
and type or select a recently used connecton address in the TUI.

### Options

- `-m`: Enables persistent session history. Data is saved in `$HOME/.dqcon/`.
- `-s`: Launches in standard REPL mode instead of the TUI.
- `-v`: Shows version info
- `-h`: Shows basic usage info

### Key Bindings (TUI)

- **Enter**: Run current command
- **Up/Down**: Navigate command history
- **Shift+Arrows**: Select text in command
- **Shift+Home/End**: Extend selection to start/end
- **Mouse Left Click/Drag**: Select text in results
- **Mouse Right Click**: Paste from clipboard
- **Ctrl+C**: Copy command/result selection (or full command if no selection)
- **Ctrl+X**: Cut selection or exit app
- **Ctrl+V**: Paste from clipboard
- **Ctrl+N**: Insert newline
- **Ctrl+R**: Open repeat menu (periodic execution)
- **Ctrl+D**: Delete the current command from history
- **Ctrl+H**: Select connection
- **Ctrl+Q**: Show help dialog

## Installation

Download the zipped binary for your architecture from the [releases](https://github.com/adotsch/dqcon/releases) page and move it to your PATH.

For example:

```bash
unzip dqcon-linux-amd64.zip -d ~/.local/bin/
```

### Linux/GLIBC Compatibility

All the Linux binaries are distribution agnostic, and they should run on almost any Linux distribution without any issues. 🤞
`dqcon` is tested and runs correctly even on ancient distros like `Centos 5` (`GLIBC 2.5`) and `Centos 6` (`GLIBC 2.12`).

## Building

The project uses a containerized build system to ensure consistency of the build process and to avoid installing toolchains on the developer's machine.

### Prerequisites

- [Docker](https://www.docker.com/)
- `make`

### Build Targets

To build for `linux/arm64` (default):
```bash
make
```

To build for all supported platforms (Linux, Windows, macOS on x86_64 and aarch64):
```bash
make cross
```

## Donations

If you like this app, please consider supporting the author with a donation in any currency/coin:

- Revolut: [@andrasdotsch](https://revolut.me/andrasdotsch)
- Bitcoin: `bc1qfanp6jt8rnrw9zjpc4pwpsd3x552rathjrmecw`
- Ethereum (and L2/EVM chains): `0xf780b169E2177d13938355bF5abE796575C1003E`
- XRP: `rPthpv7Ex7CGPc28rVKtfzazgM2MxcketY`
- Solana: `21gwoU51YPRD29vpsRYKyvNJ6xEtJSXdM7LPXrG63RtA`
- Dogecoin: `D8eSvpNkaAfhyFhxyXcaTXsVAMgERsAico`
- Zcash: `t1L1ncvwaCQhp6yziDtcrWrd7vquioswNxC`

## License

MIT
