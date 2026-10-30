**What's new in 0.2.5**
- **ProRes RAW input** (Nikon): .MOV ProRes RAW clips can be converted to N-RAW or R3D NE, previewed, and developed in the Develop tab. The conversion is not lossless and hits the bpp you pick. Decoding runs on all CPU cores. Docker and the Web UI need ffmpeg 8 or newer for ProRes RAW; the current image ships an older ffmpeg, so ProRes RAW clips work in the desktop app and the command line tool with a recent ffmpeg.
- **Convert** choice: Off, N-RAW or R3D NE, for all clips or per clip. With R3D NE chosen, only the .R3D and its .RMD are written (`--r3d-only`, `R3D_ONLY`, `WATCH_R3D_ONLY`). Skip compression is only shown when a lossless rewrap is possible.
- **.RMD sidecar** now follows RED's RSX layout, so the exposure adjustment is read correctly (+0.5 EV for ZR clips).
- **Viewer zoom**: true 1:1 at 100%, new 50% and 400% steps, and pinch to zoom on a trackpad.
- The Compress preview and thumbnails now use the same as-shot look as Develop.
- Compression targets show as one bar under the bpp slider, with the value as a share (for example Medium 55%), also on the queue cards.
- Narrow window layout fixes, and a startup fix that made the file and folder pickers unresponsive.
- 0.2.4 was skipped.

**What's new in 0.2.3**
- New **Develop** tab for quick turnaround: basic tools for developing the digital negatives (RAWs) to a 10-bit HEVC MOV in Log3G10, N-Log or Rec.709, on the GPU. Controls follow the familiar raw panels for R3D and N-RAW: white balance per clip, ISO, exposure, auto exposure, highlight roll off, tone map, contrast and more. Every clip starts from what it recorded, including its .RMD sidecar. ISO uses the same values as RED's tools (50 to 25600).
- **Rewrap** N-RAW (.NEV) to R3D NE and back, losslessly, with **Skip compression (lossless)**. An .RMD exposure sidecar is always written so the rewrapped clip looks like the original.
- **Experimental:** Z8, Z9 and Z6III clips to R3D NE, with an .RMD sidecar (+0.5 EV; +1 stop for ZR clips).
- **Constant bitrate** option for all formats.
- The app is now in **14 languages**.
- **Check for Updates...** is now in the menu bar (the Blue Compressor menu on a Mac, the Help menu on Windows and Linux), with **Check Automatically** to turn the daily check on or off. The sidebar Updates section is gone.
- Proxies are removed: Nikon cameras record their own, and the Develop tab covers quick deliverables.
- Preview fixes: the preview no longer clips the window when it is narrower than 16:9 or than the clip.
- Develop export on Windows and Linux uses an ffmpeg installed on your computer; the Mac app includes one.

**FFmpeg license and source.** The macOS app bundles an unmodified static FFmpeg 9.0.2 executable (from https://www.martin-riedl.de, with libx265) that runs as a separate program for Develop exports. It is licensed under the GNU General Public License (https://www.gnu.org/licenses/gpl-3.0.html), FFmpeg is Copyright (c) 2000-2026 the FFmpeg developers. The complete corresponding source is at https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz and the build scripts at https://ffmpeg.martin-riedl.de. On request (open an issue) we will provide that source for at least three years after we last distribute this build. See NOTICE.

**What's new in 0.2.0**
- The desktop app now checks for updates by itself and asks before installing one: Update, Later or Skip this version. It downloads, checks the signature, installs and reopens, and never interrupts a batch that is compressing (it offers to update when the queue is done). The check is anonymous: it only asks GitHub for the newest version, nothing about you or your clips is sent. Turn it off with Check for updates in the sidebar, or use Check Now.
- Docker and NAS: the Web UI shows a banner when a new version is out, with the steps to update on Synology, Docker Compose and Docker. Set `UPDATE_CHECK=false` to turn the check off. The container never updates itself.
- If you have 0.1.7 or older, install 0.2.0 by hand once. After that, updates are offered automatically.

**What's new in 0.1.7**
- One simple size control: bits per pixel (bpp). It replaces the old Q1 to Q4 quality steps. Four quick picks set it for you, each worked out from every clip's own rate:
  - **Lossless**: identical picture, files only a little smaller (same as before).
  - **Light** (default): minimal quality loss, about 25% smaller.
  - **Medium**: balanced, good for archive copies, about 45% smaller.
  - **Strong**: smallest files, for reference copies, about 65% smaller.
- Or drag the slider (0 to 5) or type a value to pick your own bpp for every clip. Nikon's own HE (3 bpp) and HE\* (5 bpp) stills rates are marked on the slider, and yellow marks show your clips' own rates. A value at or above a clip's own rate keeps that clip lossless.
- Every frame is now fitted to the size you choose, so the sizes above hold on any footage. Sizes will differ from what the old Q2, Q3 and Q4 gave, so compressed copies made with 0.1.6 or earlier will not match new ones.
- Smoother slider: the handle follows your pointer, is easier to grab, and you can click anywhere on the track. Arrow keys and the scroll wheel step by 0.1.
- More accurate source rate: a clip's bits per pixel now counts only the coded picture data.
- Command line, Docker and NAS: new `--bpp`, `--preset light|medium|strong` and `--lossless` options, and `BPP`, `PRESET`, `WATCH_BPP` and `WATCH_PRESET` settings. The watch folder now defaults to Light. Existing scripts keep working: `-q 1` to `-q 4` and `QUALITY` / `WATCH_QUALITY` still work and mean Lossless, Light, Medium and Strong.
- Thanks to RdWing for the bits per pixel idea and for *Nikon HE and HE\* RAW: Format analysis and decoding reference*, copyright (c) 2026 patrol_taking_9j (RdWing), licensed under CC BY-SA 4.0, where the bpp measure and the HE and HE\* figures come from.

**What's new in 0.1.6**
- Anamorphic de-squeeze in the quality previewer. A new De-squeeze setting (None, 1.25x, 1.33x, 1.5x, 1.55x, 1.6x, 1.8x, 2x, or a custom factor from 1 to 3) stretches the preview sideways so anamorphic footage shows in its intended shape. It applies to the before/after viewer, the minimap and the queue thumbnails, and the clip details show the resulting on-screen aspect ratio, for example "de-squeezed 1.33x, 2.36:1 on screen". Zoom, pan and the comparison line work as before.
- Display only: the raw data and the compressed clips are never changed, and size estimates are unaffected. The chosen factor is remembered with your other settings. Works in the desktop app and the Web UI.

**What's new in 0.1.5**
- Already-compressed clips are detected up front: when clips or folders are added, and again when the quality, resolution or output folder changes, clips whose compressed copy for that setting already exists are marked "Already done" right away. They are left out of the Compress button count (for example "Compress 304 clips"), the queue header shows how many are already done and how much they saved, and when everything is done the button says so. Compress again still forces a new version, and the run-time skip stays as a safety net.
- Live "saved so far" counter for the current batch, for example "Saved 412 GB so far -38% · 85 of 304 done", while running and after it finishes. Finished clips count exactly and the clip in progress is estimated; clips that were already done are counted separately in the queue header. In the Web UI the counter and the already-done clips survive a page reload.

**What's new in 0.1.4**
- Pause and resume the queue. Pause holds the clip being compressed where it is, and Resume continues it; Stop still cancels, also while paused. Works in the desktop app and the Web UI.
- Clear List button: empties the whole list when nothing is running; while compressing it removes only the finished clips.
- Safer, resumable batches: each clip is written to a temporary file and renamed only when it is complete, so an interrupted run (crash, NAS or container restart) never leaves a broken clip under the final name. Clips whose compressed copy for the same setting already exists are skipped and shown as "Already done", so adding the same folder again after an interruption only compresses what is left. Compress again still makes a new version on purpose.
- Docker and Web UI: the queue now lives on the server. Reloading or closing the page no longer loses it; the page restores the clips, progress and pause state, and you can add more clips while it runs. Pausing a Web UI job lets the watch folder keep working in the meantime.

**What's new in 0.1.3**
- AMD (OpenCL) and NVIDIA (CUDA) GPU acceleration is now enabled in both the Windows and Linux desktop app and the command line tool when downsizing to 6K or 4K, with automatic fallback to the CPU when no supported GPU or runtime is present. Both GPU paths now use a double-precision transfer curve and disable FMA contraction where the CPU does, matching CPU output bit for bit.
- Validated CUDA support on Windows 11 (RTX 4060 Ti), clearer error reporting and documentation for the NVRTC 12.x runtime requirement on Windows, and a CUDA kernel compile and load test (thanks to @Lucke-bp in PR #2).

**What's new in 0.1.2**
- Experimental GPU acceleration on Windows and Linux, in both the app and the command line tool: AMD graphics cards through OpenCL and Nvidia graphics cards through CUDA. The GPU is only used when downsizing to 6K or 4K; at the original resolution everything runs on the CPU as before. Without a supported GPU and driver it falls back to the CPU automatically.
- This is untested on real AMD and Nvidia hardware, so feedback is very welcome. If you have one of these cards:
  1. Check that the engine label in the app (or `bluecompress backends`) shows your graphics card.
  2. Compress a clip to 6K or 4K once with GPU and once with CPU at the same quality, and compare the two output files' hashes (they should be identical).
  3. Report the speed of both runs, and any crashes or errors, with your card and driver version.

**What's new in 0.1.1**
- Fixed: queue rows no longer get squashed when a large batch of clips is added (for example a few hundred). Rows keep their full height and the list scrolls, so you can remove or change settings for any clip.

Early, experimental build. Tested on one Nikon Z8 (N-RAW) clip and one Nikon ZR (R3D NE) clip on macOS (Apple silicon). Windows and Linux builds and the CUDA backend are untested. Keep your originals until you have checked the outputs.

**Downloads**
- macOS: `.dmg` (Apple silicon `arm64` or Intel `x64`)
- Windows: `-setup.exe` installer or `.msi`
- Linux: `.AppImage`, `.deb` or `.rpm`
- Command line tool, library and C header: `bluecompress-cli_*` archives

**Unsigned builds.** These builds are not signed with an Apple or Microsoft developer certificate.
- **macOS**: drag the app to Applications and open it. macOS says it cannot verify the app; click **Done**, then open **System Settings > Privacy & Security**, scroll down and click **Open Anyway**. You only need to do this once. On macOS 14 and earlier, right-clicking the app and choosing **Open** also works.
- **Windows**: SmartScreen may warn; choose **More info**, then **Run anyway**.
