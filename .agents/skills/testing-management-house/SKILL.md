---
name: testing-management-house
description: How to run and E2E-test the static "Management House" web app (Three.js 3D office + hub modules) in /home/ubuntu/pc-agent/web, including pointer-lock navigation tricks, room coordinates, and known environment limits.
---

# Testing Management House (web/)

## Serve it
```bash
cd <repo>/web && python3 -m http.server 8088
```
- House (3D office): http://localhost:8088/house/
- Module hub: http://localhost:8088/ ; modules: `/zishel/ /fx/ /showcase/ /control/ /brandkit/ /portfolio/ /business/ /card/`
- Always type the full `http://` prefix — Chrome treats `localhost:8088/house/` as a search/domain.

## Navigating the 3D house
- Click «ВОЙТИ В ДОМ» to acquire pointer lock; WASD walk, Shift run, mouse look, LMB opens a department card, `E` enters the room's mapped module, Esc releases the lock.
- Mouse look in pointer lock responds to `mouse_move` deltas; a single small move may not register visibly at low FPS — repeat the same `mouse_move` twice and wait ~2 s between steps.
- Room coords (x, z): floor 1 — reception (22,13), marketing (22,-13), sales (-22,-13), warehouse (-22,13); floor 2 — chill (-22,13), director (0,13), admin (22,-13), accounting (-22,-13). Stairs/ramp x=26..42, |z|<4.
- Room→module map lives in `web/shared/modules.js` (chill→fx, reception→card, director→control, sales→business, marketing→brandkit, warehouse→showcase, admin→portfolio, accounting→zishel).
- Debug handle exposed by the page: `window.house.{scene,camera,controls,DEPARTMENTS,teleport(x,z,y)}`. On a software GPU walking 25 units takes minutes, so use `window.house.teleport(22,13,0)` to reach a room, then do the actual interaction (aim, LMB, `E`) through the UI and say so in the report.
- Verify hover state from the DOM/crosshair text (e.g. `РЕСЕПШН — ЛКМ данные · E → BUSINESS CARD`) before pressing `E`.
- Read `window.house.camera.position` with `console.log(...)` — bare expressions returning objects/template strings sometimes come back `undefined` through CDP.

## Module assertions that are quick and concrete
- `/zishel/`: submitting with the consent checkbox unchecked must show «Нужно согласие на связь в WhatsApp.»; with it checked the page opens `wa.me/<BRAND.whatsapp>?text=...` (number lives in `web/shared/data.js`, currently the placeholder `77000000000`) and shows a «нажмите здесь» fallback link.
- `/brandkit/`: choose format «Сторис 1080×1920» → «СКАЧАТЬ PNG» writes `~/Downloads/zishel_offer_1080x1920.png`; verify with
  `python3 -c "from PIL import Image;print(Image.open('...').size)"`.
- `/control/`, `/business/`, `/portfolio/`, `/card/`, `/showcase/`, `/fx/` all render data from `web/shared/data.js` — assert one concrete number (e.g. calculator total, KPI tile) rather than "page loads".

## Known environment limits / possible gotchas
- Chrome here runs on `ANGLE (Vulkan SwiftShader)`, i.e. software WebGL: measured ~2.7 FPS in the house. Do not treat low FPS as an app regression without a real GPU; measure explicitly with a `requestAnimationFrame` counter and report the renderer string.
- The house logs many `THREE.WebGLRenderer: Texture marked for update but no image data found.` warnings (~96 in a few minutes). These are warnings, not errors; check for errors with a grep for `error|404|draco|gltf|failed` over the console dump.
- Long 3D label plates (АДМИНИСТРАЦИЯ, ОТДЕЛ ПРОДАЖ, ЧИЛАУТ-ЗОНА) can look truncated from inside the corridor because the doorway side walls occlude the plate edges — before calling it a `label()` font-fit bug, re-check the same plate straight-on from inside the room.
- Movement collision is AABB-based (`blocked()`); pushing W into a wall for several seconds is a valid no-clip test.

## Devin Secrets Needed
- none (fully static app, no auth, no external API keys).
