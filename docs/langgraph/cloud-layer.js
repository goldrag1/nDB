// ── All-papers GPU point-cloud backdrop ───────────────────────────────────
// The 3d-force-graph interactive layer maxes out at a few thousand meshes (one
// mesh + force-sim body per node). To show ALL ~6.17M papers we render them as
// a single THREE.Points cloud (one GPU draw call) added to the same scene, as a
// non-interactive backdrop. The force-graph stays the focused interactive layer
// on top (labels, hover, click, citation links).
//
// Data: GET api/view/cloud -> cloud.bin (binary, see server build_cloud_file):
//   header [magic "NDCLOUD1"(8) | u32 count | u32 0], then count x record
//   [f32 x | f32 y | f32 z | u16 field_idx | u16 size_q] (16 B/record).
// field_idx indexes the clusters array -> colored by the SAME field hue as the
// nodes. Positions are the EXACT server layout, so points coincide with nodes.
(function () {
  "use strict";

  // Match the explorer's fieldColor(): hsl(hash(field) % 360, 68%, 62%).
  function fieldHue(name) {
    let h = 0;
    for (const c of name) h = (Math.imul(h, 31) + c.charCodeAt(0)) >>> 0;
    return h % 360;
  }
  function hslToRgb(h, s, l) {
    h /= 360;
    const a = s * Math.min(l, 1 - l);
    const f = (n) => {
      const k = (n + h * 12) % 12;
      return l - a * Math.max(-1, Math.min(k - 3, Math.min(9 - k, 1)));
    };
    return [f(0), f(8), f(4)];
  }
  const OTHER_RGB = hslToRgb(fieldHue("Other"), 0.68, 0.62);

  function loadScript(src) {
    return new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = src; s.onload = resolve; s.onerror = () => reject(new Error("load " + src));
      document.head.appendChild(s);
    });
  }

  async function ensureThree() {
    if (window.THREE) return window.THREE;
    // 3d-force-graph bundles THREE privately; load our own compatible copy so
    // we can construct geometry. The WebGLRenderer draws any isObject3D added
    // to the scene, so a separate THREE copy renders fine.
    await loadScript("https://unpkg.com/three@0.150.1/build/three.min.js");
    return window.THREE;
  }

  function waitForGraph() {
    return new Promise((resolve) => {
      const t = setInterval(() => {
        if (window.Graph && typeof window.Graph.scene === "function") {
          clearInterval(t); resolve(window.Graph);
        }
      }, 150);
    });
  }

  async function fetchClusterColors() {
    // field_idx -> [r,g,b], same order the server emits (clusters array).
    try {
      const c = await (await fetch("api/view/clusters", { cache: "no-store" })).json();
      return (c.nodes || []).map((n) =>
        n.field === "Other" ? OTHER_RGB : hslToRgb(fieldHue(n.field), 0.68, 0.62)
      );
    } catch (e) { return []; }
  }

  // Parse the interleaved 16-B records into flat position/color arrays and
  // importance-sort them (prefix [0..N) = top-N most-cited). The loop + the
  // 6.17M-element Int32 sort take ~2 s — must NOT run on the main thread.
  // Self-contained (no DOM/THREE) so it doubles as the Web Worker body.
  function parseSortCore(buf, lut, other) {
    var dv = new DataView(buf);
    var count = dv.getUint32(8, true), REC = 16;
    var px = new Float32Array(count * 3), pc = new Float32Array(count * 3), imp = new Float32Array(count);
    var off = 16;
    for (var i = 0; i < count; i++, off += REC) {
      px[i * 3] = dv.getFloat32(off, true); px[i * 3 + 1] = dv.getFloat32(off + 4, true); px[i * 3 + 2] = dv.getFloat32(off + 8, true);
      var fi = dv.getUint16(off + 12, true), rgb = lut[fi] || other;
      pc[i * 3] = rgb[0]; pc[i * 3 + 1] = rgb[1]; pc[i * 3 + 2] = rgb[2];
      imp[i] = dv.getUint16(off + 14, true) / 65535;
    }
    var order = new Int32Array(count);
    for (var j = 0; j < count; j++) order[j] = j;
    order.sort(function (a, b) { return imp[b] - imp[a]; });
    var positions = new Float32Array(count * 3), colors = new Float32Array(count * 3);
    for (var k = 0; k < count; k++) {
      var s = order[k];
      positions[k * 3] = px[s * 3]; positions[k * 3 + 1] = px[s * 3 + 1]; positions[k * 3 + 2] = px[s * 3 + 2];
      colors[k * 3] = pc[s * 3]; colors[k * 3 + 1] = pc[s * 3 + 1]; colors[k * 3 + 2] = pc[s * 3 + 2];
    }
    return { positions: positions, colors: colors, count: count };
  }

  // Run parseSortCore in a worker, transferring the 95 MB buffer IN (zero copy)
  // and the sorted position/color buffers OUT. Main thread never blocks.
  function parseSortInWorker(buf, lut, other) {
    return new Promise(function (resolve, reject) {
      var src = "var parseSortCore=" + parseSortCore.toString() + ";\n" +
        "self.onmessage=function(e){var d=e.data,r=parseSortCore(d.buf,d.lut,d.other);" +
        "self.postMessage({positions:r.positions,colors:r.colors,count:r.count},[r.positions.buffer,r.colors.buffer]);};";
      var url = URL.createObjectURL(new Blob([src], { type: "text/javascript" }));
      var w = new Worker(url);
      w.onmessage = function (e) { resolve(e.data); w.terminate(); URL.revokeObjectURL(url); };
      w.onerror = function (e) { reject(new Error((e && e.message) || "worker error")); w.terminate(); URL.revokeObjectURL(url); };
      w.postMessage({ buf: buf, lut: lut, other: other }, [buf]);
    });
  }

  async function buildCloud() {
    const Graph = await waitForGraph();
    let probe;
    try { probe = await fetch("api/health", { cache: "no-store" }); } catch (e) { return; }
    if (!probe || !probe.ok) return; // LIVE mode only (static graph.json has no cloud)

    const THREE = await ensureThree();
    if (!THREE) { console.warn("cloud-layer: THREE unavailable"); return; }

    const colorLut = await fetchClusterColors();

    const resp = await fetch("api/view/cloud", { cache: "no-store" });
    if (!resp.ok) { console.warn("cloud-layer: /view/cloud", resp.status); return; }
    const buf = await resp.arrayBuffer();
    const magic = String.fromCharCode.apply(null, new Uint8Array(buf, 0, 8));
    if (magic !== "NDCLOUD1") { console.warn("cloud-layer: bad magic", magic); return; }

    // Parse + importance-sort 6.17M records OFF the main thread (worker), so the
    // ~2 s of work never freezes the UI on load. Falls back to inline parsing if
    // Workers/Blob URLs are unavailable. size_q (byte 14) is the LOD key.
    let positions, colors, count;
    try {
      ({ positions, colors, count } = await parseSortInWorker(buf, colorLut, OTHER_RGB));
    } catch (e) {
      console.warn("cloud-layer: worker parse failed, falling back inline", e);
      ({ positions, colors, count } = parseSortCore(buf, colorLut, OTHER_RGB));
    }

    const geom = new THREE.BufferGeometry();
    geom.setAttribute("position", new THREE.BufferAttribute(positions, 3));
    geom.setAttribute("color", new THREE.BufferAttribute(colors, 3));
    // On-screen point budget (the LOD). Zoom OUT = whole galaxy, worst overlap
    // → few hubs; zoom IN → budget grows, long tail fades in. Start conservative
    // because the boot frame can be fully zoomed out; camera 'change' refines.
    const MIN_DRAW = 60000, MAX_DRAW = Math.min(count, 350000);
    geom.setDrawRange(0, Math.min(count, 90000));

    // OPAQUE + depthWrite is the crash fix: 6M transparent discs with
    // depthWrite:false force every covered pixel to blend (no early-Z) →
    // fillrate wall → iGPU driver hang. Opaque points keep early-Z so hidden
    // points cost nothing.
    const mat = new THREE.PointsMaterial({
      size: 2.4,
      sizeAttenuation: true,
      vertexColors: true,
      transparent: false,
      depthWrite: true,
    });
    // Round points WITHOUT a texture. A CanvasTexture built by this file's
    // separate THREE instance trips 3d-force-graph's bundled THREE on the
    // colorSpace upload path (getPrimaries crash → render loop dies). Discard
    // fragments outside the unit disc in-shader instead: round, opaque, zero
    // texture, no cross-instance colorSpace, and cheaper than sampling a sprite.
    mat.onBeforeCompile = (shader) => {
      shader.fragmentShader = shader.fragmentShader.replace(
        "#include <clipping_planes_fragment>",
        "#include <clipping_planes_fragment>\n\tif (length(gl_PointCoord - vec2(0.5)) > 0.5) discard;"
      );
    };
    const points = new THREE.Points(geom, mat);
    points.name = "ndb-paper-cloud";
    points.renderOrder = -1;       // behind the interactive nodes
    points.frustumCulled = false;  // bounding sphere covers all points; drawRange picks the prefix
    Graph.scene().add(points);

    // Cap the device pixel ratio: a HiDPI panel renders up to 4× the fragments,
    // and fragments (not vertices) are what the integrated GPU chokes on.
    try {
      const r = Graph.renderer && Graph.renderer();
      if (r && r.setPixelRatio) r.setPixelRatio(Math.min(window.devicePixelRatio || 1, 1.5));
    } catch (e) {}

    // ── Camera-distance draw budget (the LOD) ──────────────────────────────
    // Fixed on-screen point budget, Google-Earth style. Because points are
    // importance-sorted, setDrawRange(0,N) always draws the top-N most-cited —
    // no re-upload, no per-frame CPU. budget grows as the camera moves IN.
    let base = 0;
    const applyBudget = () => {
      const cp = Graph.cameraPosition();
      const d = Math.hypot(cp.x || 0, cp.y || 0, cp.z || 0) || 1;
      if (!base) base = d;
      const budget = Math.max(MIN_DRAW, Math.min(MAX_DRAW, Math.round(MAX_DRAW * (base / d))));
      geom.setDrawRange(0, budget);
      if (window.__cloud) window.__cloud.drawn = budget;
    };
    setTimeout(applyBudget, 300);

    window.__cloud = {
      points, count, geom, mat, drawn: 0,
      setBudget: (n) => geom.setDrawRange(0, Math.max(0, Math.min(count, n | 0))),
      setSize: (s) => { mat.size = s; },
    };
    console.log("cloud-layer: " + count.toLocaleString() + " papers loaded, importance-LOD budget " + MIN_DRAW + "–" + MAX_DRAW);

    const controls = Graph.controls && Graph.controls();
    if (controls && controls.addEventListener) {
      controls.addEventListener("change", () => { clearTimeout(window.__cloudT); window.__cloudT = setTimeout(applyBudget, 80); });
    }
  }

  if (document.readyState === "complete") setTimeout(buildCloud, 1500);
  else window.addEventListener("load", () => setTimeout(buildCloud, 1500));
})();
