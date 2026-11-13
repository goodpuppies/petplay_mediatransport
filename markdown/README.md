# Implementation notes

The first consumer is PetPlay's userspace headless OpenVR capture layer. Current design and
validation history live in the parent project's `markdown/networked-video.md`.

Keep reusable transport, receiver, platform-adapter, and codec plans here. Keep SteamVR and
OpenVR device/compositor plans in PetPlay or `petplay_ovrdriver`.

Cross-platform adapters and generic media feeds are planned scope, not implemented capability.
