# NALSense test fixtures

`hevc-idr-64x64.h265` is a project-generated, one-frame HEVC Main-profile
Annex-B stream. It contains a uniform gray 64x64 YUV 4:2:0 frame and no camera
or third-party media.

It was generated with FFmpeg 9.0.1 and libx265 on 2026-08-28:

```sh
ffmpeg \
  -f lavfi \
  -i 'color=c=gray:s=64x64:r=25:d=0.04' \
  -frames:v 1 \
  -an \
  -c:v libx265 \
  -preset medium \
  -pix_fmt yuv420p \
  -g 1 \
  -bf 0 \
  -x265-params 'keyint=1:min-keyint=1:scenecut=0:bframes=0:open-gop=0:repeat-headers=1:log-level=error' \
  -f hevc \
  hevc-idr-64x64.h265
```

SHA-256:

```text
b2c3a2430d320c4ea9cb8e736d017534357ae609f17b926430b408da6a51376e  hevc-idr-64x64.h265
```

## Replay streams

Replay tests generate streams on demand using FFmpeg with libx264 and libx265.
The ignored `.cache/nalsense/` directory caches them by encoder arguments;
delete it to regenerate with a different FFmpeg version.
