# Security

## Your Discord bot token

- Lantern never ships with a token. It is pasted into the window (or the Docker web page) and stored only in `lantern-settings.json`, next to `Lantern.exe` or in the Docker config folder.
- Do not upload, screenshot or share `lantern-settings.json`. The file is listed in `.gitignore` so it is not included with the code.
- If your token leaks, open the [Discord Developer Portal](https://discord.com/developers/applications), select your app, go to **Bot**, and press **Reset Token**. The old token stops working at once.

## Files that may contain private data

| File | Contains |
| --- | --- |
| `lantern-settings.json` | Bot token, owner ID, library folder paths |
| `lantern-data.json` | Server IDs, saved playlists, history (may include local file paths) |
| `lantern-library.json` | Paths and tags of your music files |
| `lantern-log*.txt`, `lantern-crash.txt` | Activity log |

None of these are sent to Discord. Don't attach them to public bug reports without removing tokens, IDs and paths.

## What Lantern protects against

- Links to `localhost`, private LAN ranges and similar are refused for everyone except the owner, so server members can't make Lantern fetch things from your network.
- Discord users only ever see track names, never local file paths.

## Reporting a vulnerability

Please open a private GitHub security advisory on this repository (Security tab > Report a vulnerability) rather than a public issue.
