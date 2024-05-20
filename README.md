# Hullcaster

Hullcaster is a terminal-based podcast manager, built in Rust. It provides a terminal UI (i.e., an ncurses-like interface) to allow users to subscribe to podcast feeds, and sync feeds to check for new episodes. Episodes may be downloaded locally, played with an external media player, and marked as played/unplayed. Keybindings and other options are configurable via a config file.

## Note

This is a fork of [shellcaster](https://github.com/jeff-hughes/shellcaster), which is no longer maintained. Currently, I am planning to implement the features states in `TODO.md`, while learning `rust` at the same time.

## Installing hullcaster

### On Linux distributions and MacOS

Currently the only option is to build from source.

First, ensure you have installed the necessary dependencies:

* rust
* gcc
* pkg-config
* libsqlite3-dev

**Notes:**

* The names of these dependencies may be slightly different for your system. For `libsqlite3-dev`, you are looking for the development headers for SQLite, which may be separate from the runtime package (e.g., with a `-dev` suffix).
* If you enable the "native_tls" feature of hullcaster (disabled by default), you will also need `libssl-dev`, the development headers for OpenSSL (not needed on MacOS).
* If you enable the "sqlite-bundled" feature of hullcaster (disabled by default), `pkg-config` and `libsqlite3-dev` are not necessary.

Next, you can clone the Github repo and compile it yourself:

```bash
git clone https://github.com/gilcu3/hullcaster.git
cd hullcaster
cargo build --release  # add or remove any features with --features

# for MacOS or Linux
sudo cp target/release/hullcaster /usr/local/bin/

# or for Linux, no root permissions
cp target/release/hullcaster ~/.local/bin
```

See below for the list of available features when compiling.



## Running hullcaster

Easy-peasy! In your terminal, run:

```bash
hullcaster
```

Note that if you installed hullcaster to a different location, ensure that this location has been added to your `$PATH`:

```bash
export PATH="/path/to/add:$PATH"
```

## Importing/exporting podcasts

Hullcaster supports importing OPML files from other podcast managers. If you can export to an OPML file from another podcast manager, you can import this file with:

```bash
hullcaster import -f /path/to/OPML/file.opml
```

If the `-r` flag is added to this command, it will overwrite any existing podcasts that are currently stored in hullcaster. You can also pipe in data to `hullcaster import` from stdin by not specifying the `-f <file>`.

You can export an OPML file from hullcaster with the following command:

```bash
hullcaster export -f /path/to/output/file.opml
```

You can also export to stdout by not specifying the `-f <file>`; for example, this command is equivalent:

```bash
hullcaster export > /path/to/output/file.opml
```

## Configuring hullcaster

If you want to change configuration settings, the sample `config.toml` file can be copied from [here](https://raw.githubusercontent.com/gilcu3/hullcaster/master/config.toml). Download it, edit it to your fancy, and place it in the following location:

```bash
# on Linux
mkdir -p ~/.config/hullcaster
cp config.toml ~/.config/hullcaster/

# on MacOS
mkdir -p ~/Library/Preferences/hullcaster
cp config.toml ~/Library/Preferences/hullcaster/
```

Or you can put `config.toml` in a place of your choosing, and specify the location at runtime:

```bash
hullcaster -c /path/to/config.toml
```

The sample file above provides comments that should walk you through all the available options. If any field does not appear in the config file, it will be filled in with the default value specified in those comments. The defaults are also listed below, for convenience.

### Configuration options

**download_path**:

* Specifies where podcast episodes that are downloaded will be stored.
* Defaults:
  * On Linux: $XDG_DATA_HOME/hullcaster/ or $HOME/.local/share/hullcaster/
  * On Mac: $HOME/Library/Application Support/hullcaster/
  * On Windows: C:\Users\\**username**\AppData\Local\hullcaster\

**play_command**:

* Command used to play episodes. Use "%s" to indicate where file/URL will be entered to the command. Note that hullcaster does *not* include a native media player -- it simply passes the file path/URL to the given command with no further checking as to its success or failure. This process is started *in the background*, so be sure to send it to a program that has GUI controls of some kind so you have control over the playback.
* Default: "vlc %s"

**download_new_episodes**:

* Configures what happens when new episodes are found as podcasts are synced. Valid options:
  * "always" will automatically download all new episodes;
  * "ask-selected" will open a popup window to let you select which episodes to download, with all of them selected by default;
  * "ask-unselected" will open a popup window to let you select with episodes to download, with none of them selected by default;
  * "never" will never automatically download new episodes.
* Default: "ask-unselected"

**simultaneous_downloads**:

* Maximum number of files to download simultaneously. Setting this too high could result in network requests being denied. A good general guide would be to set this to the number of processor cores on your computer.
* Default: 3

**max_retries**:

* Maximum number of times to retry connecting to a URL to sync a podcast or download an episode.
* Default: 3

#### Default keybindings

| Key     | Action         |
| ------- | -------------- |
| ?       | Open help window |
| Arrow keys / h,j,k,l | Navigate menus |
| Shift+K | Up 1/4 page |
| Shift+J | Down 1/4 page |
| PgUp    | Page up |
| PgDn    | Page down |
| a       | Add new feed |
| q       | Quit program |
| s       | Synchronize selected feed |
| Shift+S | Synchronize all feeds |
| Enter / p | Play selected episode |
| m       | Mark selected episode as played/unplayed |
| Shift+M | Mark all episodes as played/unplayed |
| d       | Download selected episode |
| Shift+D | Download all episodes |
| x       | Delete downloaded file |
| Shift+X | Delete all downloaded files |
| r       | Remove selected feed/episode from list |
| Shift+R | Remove all feeds/episodes from list |
| 1       | Toggle played/unplayed filter |
| 2       | Toggle downloaded/undownloaded filter |

**Note:** Actions can be mapped to more than one key (e.g., "Enter" and "p" both play an episode), but a single key may not do more than one action (e.g., you can't set "d" to both download and delete episodes).

#### Customizable colors

You can set the colors in the app with either built-in terminal colors or (provided your terminal supports it) customizable colors as well. See the "colors" section in the [config.toml](https://github.com/gilcu3/hullcaster/blob/master/config.toml) for details about how to specify these colors!

## Syncing without the UI

Some users may wish to sync their podcasts automatically on a regular basis, e.g., every morning. The `hullcaster sync` subcommand can be used to do this without opening up the UI, and does a full sync of all podcasts in the database. This could be used to set up a cron job or systemd timer, for example. Please refer to the relevant documentation for these systems for setting it up on the schedule of your choice.
