# Hullcaster

Hullcaster is a terminal-based podcast manager, built-in Rust. It provides a
terminal UI (i.e., an ncurses-like interface) to allow users to subscribe to
podcast feeds, and sync feeds to check for new episodes. Episodes may be
downloaded locally, played with an internal or external media player, and marked
as played/unplayed. Key bindings and other options are configurable via a config
file.

[![asciicast](https://asciinema.org/a/LZ4C8AY4dgaZRtlQGblI7wo2b.svg)](https://asciinema.org/a/LZ4C8AY4dgaZRtlQGblI7wo2b)

## Note

This is a fork of [shellcaster](https://github.com/jeff-hughes/shellcaster),
which is no longer maintained. Currently, I am planning to implement the
features stated below, while learning `rust` at the same time.

### Planned changes

- [x] Option to avoid marking as read when playing
- [x] Syncing with the gpodder API
- [x] Add playing queue
- [x] Internal Player using [rodio](https://github.com/RustAudio/rodio)
- [x] Show key bindings in a bar on the bottom
- [x] Migrate to [ratatui](https://ratatui.rs/)
- [x] Add option to sync automatically on start, enabled by default
- [ ] Add periodic synchronization
- [ ] Migrate from threads to tokio

## Installing hullcaster

### Archlinux

The package is available in the `AUR` [hullcaster-git](https://aur.archlinux.org/packages/hullcaster-git).

```bash
paru -S hullcaster-git
```

### NixOS / Nix

With [flakes](https://wiki.nixos.org/wiki/Flakes) enabled, run:

```bash
nix run github:gilcu3/hullcaster
```

### On Linux distributions

### cargo-binstall

```bash
cargo binstall --git https://github.com/gilcu3/hullcaster hullcaster
```

### Build from source

First, ensure you have installed the necessary dependencies: `rust`, `gcc`,
`pkgconf`, `sqlite` (package names in Archlinux).

Next, you can clone the Github repo and compile it yourself:

```bash
git clone https://github.com/gilcu3/hullcaster.git
cd hullcaster
cargo build --release --locked # add or remove any features with --features

# no root permissions
cp target/release/hullcaster ~/.local/bin
```

## Running hullcaster

In your terminal, run:

```bash
hullcaster
```

Note that if you installed hullcaster to a different location, ensure that this
location has been added to your `$PATH`:

```bash
export PATH="/path/to/add:$PATH"
```

## Importing/exporting podcasts

Hullcaster supports importing OPML files from other podcast managers. If you can
export to an OPML file from another podcast manager, you can import this file
with:

```bash
hullcaster import -f /path/to/OPML/file.opml
```

If the `-r` flag is added to this command, it will overwrite any existing
podcasts that are currently stored in hullcaster. You can also pipe in data to
`hullcaster import` from stdin by not specifying the `-f <file>`.

You can export an OPML file from hullcaster with the following command:

```bash
hullcaster export -f /path/to/output/file.opml
```

You can also export to stdout by not specifying the `-f <file>`; for example,
this command is equivalent:

```bash
hullcaster export > /path/to/output/file.opml
```

## Configuring hullcaster

If you want to change configuration settings, the sample `config.toml` file can
be copied from
[config.toml](https://raw.githubusercontent.com/gilcu3/hullcaster/master/config.toml).
Download it, edit it to your fancy, and place it in the following location:

```bash
# on Linux
mkdir -p ~/.config/hullcaster
cp config.toml ~/.config/hullcaster/
```

Or you can put `config.toml` in a place of your choosing, and specify the
location at runtime:

```bash
hullcaster -c /path/to/config.toml
```

The sample file above provides comments that should walk you through all the
available options. If any field does not appear in the config file, it will be
filled in with the default value specified in those comments.

### Default key bindings

| Key                               | Action                                   |
|-----------------------------------|------------------------------------------|
| ?                                 | Open help window                         |
| Arrow keys / h,j,k,l              | Navigate menus                           |
| Shift+K                           | Up 1/4 page                              |
| Shift+J                           | Down 1/4 page                            |
| PgUp                              | Page up                                  |
| PgDn                              | Page down                                |
| a                                 | Add new feed                             |
| q                                 | Quit program                             |
| s                                 | Synchronize selected feed                |
| Shift+S                           | Synchronize all feeds                    |
| Shift+A                           | Synchronize with gpodder                 |
| Enter                             | Play selected episode/open sel. podcast  |
| m                                 | Mark selected episode as played/unplayed |
| Shift+M                           | Mark all episodes as played/unplayed     |
| d                                 | Download selected episode                |
| Shift+D                           | Download all episodes                    |
| x                                 | Delete downloaded file                   |
| Shift+X                           | Delete all downloaded files              |
| r                                 | Remove selected feed                     |
| e                                 | Push episode in queue                    |
| u                                 | Show/hide Unread list of episodes        |
| Tab                               | Switch selected panel                    |
| Esc                               | Go to previous view                      |
| Space                             | Play/Pause currently playing episode     |
| Ctrl + Up/Down                    | Change order of episodes in the queue    |
| Shift+P                           | Play selected episode with external player|
<!-- These are not currently implemented
| 1                                 | Toggle played/unplayed                   |
| 2                                 | Toggle downloaded/not downloaded filter  |
-->

**Note:** Actions can be mapped to more than one key, but a single key may not do more than one action (e.g., you
can't set "d" to both download and delete episodes).

#### Customizable colors

You can set the colors in the app with either built-in terminal colors or
(provided your terminal supports it) customizable colors as well. See the
"colors" section in the
[config.toml](https://github.com/gilcu3/hullcaster/blob/master/config.toml) for
details about how to specify these colors!

## Syncing without the UI

Some users may wish to sync their podcasts automatically on a regular basis,
e.g., every morning. The `hullcaster sync` subcommand can be used to do this
without opening up the UI, and does a full sync of all podcasts in the database.
This could be used to set up a cron job or systemd timer, for example. Please
refer to the relevant documentation for these systems for setting it up on the
schedule of your choice.
