# CD Player

Super simple Rust web app / docker container for playing physical CDs via a nice web interface. Supports audio CDs and mp3/flac data CDs, autoplay, and supports sending webhooks when playback starts or ends to control other equipment.

<img width="997" height="855" alt="image" src="https://github.com/user-attachments/assets/04716f6e-6f0f-450c-bd77-8a088887deb1" />


## Features

- **Auto-play** an audio CD when it's inserted, or the MP3/FLAC files on a data CD (in path order, with titles and art from their tags, or a `cover.jpg`/`folder.jpg` beside them). A disc that's already in when the drive appears (after a reboot or replug) waits for Play instead.
- **Track info and cover art** from [MusicBrainz](https://musicbrainz.org) and the [Cover Art Archive](https://coverartarchive.org), looked up by disc ID (falling back to a match on track lengths) and cached in the data volume.
- **Web remote** on port 42781: play/pause, previous/next track, stop, eject, a progress bar you can click to seek, and a track list you can tap to jump to a track.
- **Volume control** that scales only the CD player's own audio, so the system volume (and AirPlay's level) is untouched. It's remembered across restarts.
- **Software eject**, since drives like the Apple SuperDrive have no eject button. Optionally ejects automatically when the disc finishes.
- **Start/stop webhooks**, configured from the remote's Settings panel. `start` fires only once audio is actually playing, so a failed start never powers your receiver on; `stop` fires on stop, eject, end of disc, errors, and container shutdown. Each is a `POST` with body `{"event": "start"}` or `{"event": "stop"}`.
- **Home Assistant media player over MQTT**: set an MQTT broker in Settings and the CD player publishes as a [Shairport Sync](https://github.com/mikebrady/shairport-sync) player on the same topic, so the [hass-shairport-sync](https://github.com/parautenbach/hass-shairport-sync) integration shows what's playing (with cover art) and its play/pause/next/previous/stop/volume buttons control the CD. Since AirPlay and the CD can't play at once, they can share one Home Assistant player.
- **Home Assistant Disc sensor and Eject button**: with an MQTT broker set, a "CD Player" device also appears in Home Assistant through MQTT discovery, with nothing to configure there: `binary_sensor.cd_player_disc` is on while a disc is in the drive, and `button.cd_player_eject` ejects it. Handy for a dashboard Eject button that only shows while there's a disc.
- **Apple USB SuperDrive support**: the drive is sent Apple's wake-up command whenever it appears, so it takes discs on non-Mac hardware.

The sound card is held exclusively while a CD plays. If something else (e.g. an AirPlay receiver like shairport-sync) is using it, playback fails with a message instead of fighting over it.

## Quick Start with Docker Compose

```yaml
services:
  cdplayer:
    image: akshaykannan/cdplayer
    ports:
      - "42781:42781"
    devices:
      - /dev/sr0:/dev/sr0
      - /dev/snd:/dev/snd
    environment:
      - AUDIO_DEVICE=plughw:CARD=PCH,DEV=0
    volumes:
      - ./data:/data
    restart: unless-stopped
```

Then `docker compose up -d` and visit `http://<host>:42781`.

The drive must be plugged in when the container starts (Docker won't start it otherwise); unplugging and replugging while it runs is fine.

## Environment Variables

- `AUDIO_DEVICE` - ALSA output device (default `default`). Naming the card, like `plughw:CARD=PCH,DEV=0`, keeps the right output selected even if cards enumerate in a different order after a reboot. List cards with `aplay -l`; the name is the one in brackets.
- `CD_DEVICE` - The drive's device (default `/dev/sr0`).
- `DATA_DIR` - Where settings and cached album info are stored (default `/data`).
- `PORT` - HTTP port (default `42781`).

There's no authentication, so keep it on your LAN.

## Home Assistant

Create two automations with **Webhook** triggers (e.g. IDs `cdplayer-start` and `cdplayer-stop`) that turn your receiver on and off, then paste their URLs — `http://<home-assistant>:8123/api/webhook/cdplayer-start` and so on — into the remote's Settings panel. The **Test** buttons send a webhook to whatever URL is typed in, so you can check them before saving.

## Development

```bash
# Needs Rust and the ALSA headers (libasound2-dev on Debian/Ubuntu, alsa-lib-dev on Alpine).
cargo test
cargo run   # set CD_DEVICE / AUDIO_DEVICE / DATA_DIR as needed
```

Pushes to `main` run the tests and publish a multi-arch (amd64 + arm64) image to Docker Hub.
