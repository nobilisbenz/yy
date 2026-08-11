---
id: obs-follow
title: "OBS cursor follow"
topic: obs
---

# OBS cursor follow {#root}

Crop the capture to a region that tracks the pointer.

## Smoothing the crop {#smoothing}

contradicts:: [[obs-follow#naive]]

Apply exponential damping to the target rectangle, then update the transform once per
frame. Without damping the view snaps and reads as broken.

## The naive version {#naive}

Move the crop rectangle straight to the pointer on every frame. This jitters badly.
