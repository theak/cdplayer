# CD Player

Turn a Linux box with a USB CD drive into a CD player: insert an audio CD and it starts playing through the machine's sound card. A small dark-mode web remote handles play/pause, previous/next, stop, and eject, and optional webhooks fire when playback starts and stops — e.g. Home Assistant automations that power a receiver on and off.

It's a single Rust binary that talks to the hardware directly: the drive through Linux cdrom/SCSI ioctls (no udev rules or host packages needed), and the sound card through ALSA. The Docker image is ~24MB.

## Features

- **Auto-play** an audio CD when it's inserted. A disc that's already in when the drive appears (after a reboot or replug) waits for Play instead.
- **Web remote** on port 42781: play/pause, previous/next track, stop, eject, track progress.
- **Software eject**, since drives like the Apple SuperDrive have no eject button. Optionally ejects automatically when the disc finishes.
- **Start/stop webhooks**, configured from the remote's Settings panel. `start` fires only once audio is actually playing, so a failed start never powers your receiver on; `stop` fires on stop, eject, end of disc, errors, and container shutdown. Each is a `POST` with body `{"event": "start"}` or `{"event": "stop"}`.
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
- `DATA_DIR` - Where settings are stored (default `/data`).
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
