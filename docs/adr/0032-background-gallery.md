# ADR-0032: The background gallery: a daemon-owned store of bundled and imported images, listed, imported and removed over D-Bus

- **Status:** Accepted (2026-11-12), implemented in daemon 0.39.0 (`crates/omarchy-broadcastd`, `crates/obc-ipc`, `crates/omarchy-broadcast`), D-Bus revision 15. Builds on [ADR-0011](0011-daemon-lifecycle-and-dbus-api.md) (the D-Bus API and its revisions), [ADR-0012](0012-video-effects-and-maxine-backend.md) (the background image and its checks, black rather than the raw camera image), [ADR-0026](0026-config-d-and-managed-config.md) (the daemon never writes `config.toml`) and [ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md) (the fallback image). It changes no earlier decision. *Amended by [ADR-0035](0035-video-backgrounds.md) (daemon 0.43.0, D-Bus revision 18): the gallery also holds videos, and the sections "Stills now, video backgrounds later" and "Known limits" below describe revision 15 as it was.*

## Context
- The `replace` effect needs an image, and until now the only way to give it one was a path: `SetBackgroundImage`, `omarchy-broadcast effect replace --image PATH`, or `[effect] background_image`. A user had to have a file, know where it was, and keep it there. Nothing came with the package, so a fresh install of `replace` had nothing to choose from.
- A path is a weak reference. The daemon reads the file when the effect starts, so a file that was moved, renamed or deleted breaks the choice later, at the next camera start, with the effect failing (and, by default, black frames) instead of at the moment of the choice.
- A client that wants to offer a choice of images, such as the bar's panel, needs a list with names and sizes, a way to add an image, and a way to remove one. Without a shared place to do it, each client would invent its own folder and its own checks, and the CLI would have no equivalent.
- The daemon already has the checks for a background image (format from the content, 32 MiB, 8192x8192 pixels, a complete first image, a decoder for the format) in the code that the `replace` effect uses, so a gallery that stores only images that pass them can never offer a file the effect then refuses for a reason known at the time of the import.
- The CLI's JSON output has a line cap of 4096 bytes ([cli-json.md](../cli-json.md#general-rules)) that every line of every command keeps, so a list of unbounded length cannot be one line.
- The images shown by the privacy fallback ([ADR-0027](0027-output-modes-frame-generation-and-privacy-fallback.md)) have the same need: a place to keep them and a way to list them.
- The project may ship only what it may redistribute. An image bundled with the package needs a recorded origin and a licence the project can pass on.

## Decision
**The daemon owns a gallery of images in two read and write tiers, a bundled read-only one and the user's, and offers it over D-Bus (revision 15): `GetMedia` lists it, `ImportMedia` copies a checked file into it, `RemoveMedia` removes a user's item, and `MediaSerial` counts its changes. The CLI's `media` command and a panel are clients of these members.**

### Where the files live
- **The bundled gallery**, `/usr/share/omarchy-broadcast/media/<kind>/`, installed by the package and read only. It holds the images that the repository keeps in `assets/media/backgrounds`: six procedural stills made by `scripts/gen-backgrounds.sh`. A `SOURCES.toml` in the directory records, for each file, its title, author, licence (an SPDX expression), source (a URL, or the script that makes it), retrieval date and any edit. **Only the project's own work or images licensed CC0-1.0 may be bundled**, with the origin recorded; a test of the repository checks the record against the files. The daemon uses the title as the item's label and reads nothing else from the file.
- **The user's gallery**, `$XDG_DATA_HOME/omarchy-broadcast/media/<kind>/`, else `$HOME/.local/share/omarchy-broadcast/media/<kind>/`. It is written by the daemon alone. The kinds are `background` (directory `backgrounds`) and `away` (directory `away`).
- The two are separate directories: the bundled files are the package's and the user's are the user's, so the daemon can refuse to remove a bundled item by its place.

### The daemon is the single writer
`ImportMedia(kind, path)` checks the file with the checks of a background image and stores a copy; `RemoveMedia(path)` removes one. The daemon does the work, as the user it runs as, so the checks are the same for the panel, the CLI and a script, the CLI has the same capability as the panel, and there is one writer to the directory. A refusal is an `InvalidArgument` with the reason, and the file never lands in the gallery.

### Import copies
- The stored file is a copy, so the choice survives the original being moved or deleted. The gallery's own path is what a client gives to `SetBackgroundImage`; `SetBackgroundImage` itself is unchanged and still takes any readable path.
- A file whose bytes are already in the user's directory of that kind is not copied again: its path is returned and `MediaSerial` does not move.
- The name is the source's, made safe (white space collapsed, control and invisible Unicode format characters and `/` removed, leading dots dropped, at most 96 bytes), with the extension of the format found from the content. An existing name is never overwritten: a numeric suffix is added.
- The copy is written to a hidden temporary file in the same directory, synced, hard-linked to its final name, and the directory synced, so a listing never shows a partial file, and a file that exists under a name is complete. A temporary file left behind by an interrupted import is removed by a later one after ten minutes.

### What the user does in the panel
The plugin does not draw a grid of its own. To **choose** an image it hands the two directories of `GetMedia` to Omarchy's image picker, which shows the images with their labels, and selects the path that comes back. To **add** an image it uses the desktop's file chooser (the file chooser portal) and then calls `ImportMedia` (the CLI's `media import`), and selects the result. A removal is a separate, deliberate action, not a click in the picker.

### Removal is refused while the configuration names the file
`RemoveMedia` refuses a bundled item, a path that is not a regular file directly in a kind directory of the user's gallery, and any file that `[effect] background_image` or `[effect] fallback_image` names, with the key in the message. The daemon never writes `config.toml` ([ADR-0026](0026-config-d-and-managed-config.md)), so it cannot change that setting for the user, and deleting the file it points to would break the next start of the effect, or the privacy fallback, silently. When the file is the run-time background, that choice is cleared first, as `SetBackgroundImage` with an empty path does, and a refusal of the clear (while `replace` is in force with no image in the configuration) refuses the removal with its reason. The configuration check, the clear and the removal are one step of the daemon's controller, so a `SetBackgroundImage` cannot choose the file in between; if the removal then fails, the cleared choice is restored.

### Stills now, video backgrounds later
> *Amendment ([ADR-0035](0035-video-backgrounds.md), daemon 0.43.0, D-Bus revision 18): video backgrounds are implemented. A video file (MP4, M4V, MOV, MKV or WebM holding H.264, HEVC, VP9 or AV1, 8-bit 4:2:0, at most 512 MiB and 3840x2160) is accepted by `ImportMedia` of a background, listed with type `video`, its size, `duration_ms` and `poster`, and chosen with `SetBackgroundImage` like an image; an animated GIF, WebP or APNG is transcoded at import into a VP9 WebM, which is the item, and needs ffmpeg. **Imported videos are stored as they are, found again by the SHA-256 of their content. For playback in system memory the daemon may keep a derived VP9 copy (at most 1920x1080, 30 fps) in its cache. The copy is never the gallery item; it is rebuilt when needed and deleted with the item.** The per-frame background is module ABI 1.4. The away image stays a still. What follows is the first step as it was decided.*

The first step handles still images: PNG, JPEG and still WebP (an animated WebP is refused; WebP needs `webpdec` of gst-plugins-bad, an optional dependency, and without it a WebP file is refused with a reason that names it). The item carries `type`, `duration_ms` and `poster` so that a later revision can add videos without changing the reply: a video file found in a gallery directory is listed with type `video`, and `ImportMedia` refuses it with `videos are not supported yet`. A video background would be a **per-frame background through the module ABI** (the host gives a module a new background frame for each frame, instead of one image once), which is a change of the ABI and of the effect element and has its own decision.

### The JSON is one line per item
`media --json` prints a `media` line (the kind, the serial, the two directories and the number of items), then one `media_item` line for each item. Every line stays within the 4096-byte cap of the CLI's JSON whatever the gallery's size. Paths are never clipped, because a client opens them and a clipped path names no file: an item whose paths do not fit is left out, with `items_truncated` saying so, and at most 500 items are printed. The bounds are in [cli-json.md](../cli-json.md#media-media_item-and-media_import-lines).

### Revision 15
`GetMedia`, `ImportMedia`, `RemoveMedia` and the property `MediaSerial` are added, and nothing else. `MediaSerial` starts at 1 at each daemon start and goes up after an import that stored a file and after a removal, with its `PropertiesChanged`. `GetMedia` reads the serial before it lists, so a change made during the call is announced after the list that misses it. The settings tree is unchanged: the background stays a method, since the schema describes values the daemon computes and a path is not one of its types.

## Consequences
- A fresh install has backgrounds to choose from, with their origin recorded, and a user's own images stay chosen when the originals move.
- Every client has the same capability: `omarchy-broadcast media` and a script can do what the panel does, and the checks are not copied into each one.
- A client reads the list when it opens and when `MediaSerial` differs. It never polls and never reads the directories itself.
- The bundled set is a small set of procedural stills. A photograph may be added only if it is CC0-1.0 and its source recorded.
- The daemon holds the files of a user's gallery, and a removal needs a running daemon (`media remove` may start it through D-Bus activation).
- `SetBackgroundImage` still accepts a path outside the gallery, so a script that sets a path keeps working. The effect's failure when such a file goes away is what it was.
- The user's gallery counts toward the user's disk (it has no quota, see below).

### Known limits
- **Stills only** (revision 15; lifted in revision 18, see the amendment above). A video is refused on import, and a video file found in a directory is listed but cannot be used as a background yet.
- **`MediaSerial` counts the daemon's changes only.** A file put into or taken out of the user's directory by other means is seen at the next `GetMedia`, but the serial does not move for it, and it restarts at 1 with the daemon, so a client compares it only within one daemon run.
- **Imports take no lock.** Two imports of the same file made at the same moment may both store a copy; the next import of it finds one of them.
- **Duplicates are found by content, in the user's directory of the kind.** A file with the bytes of a bundled item, or of an item of the other kind, is stored.
- **The copy needs a file system with hard links.** The import links the temporary file to its final name; on a file system without hard links the import fails with `Failed` and the reason. The usual file systems of `$XDG_DATA_HOME` have them.
- **No quota.** An import is bounded to 32 MiB per file, not in total; a gallery directory is not limited in size or in the number of files. A list is bounded to 500 items in the CLI's JSON, not in D-Bus.
- **The listing does not decode.** The size comes from the header and a WebP file is listed on a machine without `webpdec`; the refusal comes at import, or when the image is chosen.
- **A path that is not UTF-8 cannot be carried by D-Bus.** Such an item is left out of the list. The name of a stored file is made from the source's name, converted to UTF-8 where it is not, so a stored name always is.
- **Only the bundled gallery of `background` has files.** No away image is bundled.

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| **The plugin copies the file into a folder** (a `cp` from the panel, the daemon reading the folder) | Two writers to one directory, and no validation before a file lands: the first error would come when the effect starts. The CLI and scripts would have no equivalent, so the capability would be the panel's alone, and the panel would carry the copy rules (names, duplicates, partial writes) that the daemon needs anyway |
| **Referencing files in place** (the gallery is a list of paths) | The file may be moved or deleted, and the choice then breaks at the next start of the effect. The daemon would have to keep a list of paths of its own, a second store to keep consistent with the files, for no benefit over a copy |
| **One unbounded JSON line for the list** | It would pass the 4096-byte cap of every CLI line with a few dozen long paths, so either the cap or the list would give. A line per item keeps the cap, lets a client read item by item, and the `media` line says how many follow |
| **An in-panel grid of thumbnails** instead of Omarchy's picker | A second image browser would be code to write and keep, when Omarchy's image picker already shows labelled images and takes the two directories. The panel stays small |
| **Drag and drop into the panel now** | The panel closes when focus moves to a file manager, nothing in Omarchy uses file drops on its first-party panels, and it cannot be verified headless. It is deferred until Omarchy supports it; the choice and the add by chooser cover the use |
| **Removal that edits the configuration** (clear `background_image` in `config.toml` when its file is removed) | The daemon never writes `config.toml` ([ADR-0026](0026-config-d-and-managed-config.md)), and a hidden change to a file the user owns is worse than a refusal that says which key to change |
| **Check, clear and remove as separate calls of the client** | A `SetBackgroundImage` could choose the file between them. One step in the controller closes the gap |
| **Bundling photographs from sources with their own licences** | Licences that restrict redistribution (a per-project subscription, or a site's own terms that are not CC0) cannot be passed on by a package. Procedural images are the project's own work, and a CC0 photograph is allowed with its source recorded |

## What would change this decision
- An Omarchy picker or panel component that offers file drops, or a way to verify the layer-shell panel's drop area: drag and drop would be built on `ImportMedia`, with no change to the daemon.
- A per-frame background in the module ABI: video backgrounds would be imported (with a poster, a duration and a codec check) and listed with the type `video`, with a new decision for the ABI and the effect element. The reply already has the keys.
- A gallery large enough to need paging or a quota: `GetMedia` would take a range or a count, and the daemon would limit the size of the user's gallery.
- A client that needs changes made by hand to be seen at once: a watch on the directories would raise `MediaSerial` for them.
- A package manager's own rule for what may be bundled that differs from CC0-1.0 or the project's own work: the bundled rule would follow it.
