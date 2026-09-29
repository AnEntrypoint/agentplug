(async () => {
  const within = (promise, ms) => Promise.race([promise, new Promise((_, reject) => setTimeout(() => reject(new Error('timeout')), ms))]);
  const report = {};
  const canvas = (document.body || document.documentElement).appendChild(document.createElement('canvas'));
  canvas.width = canvas.height = 8;
  let gl = null;
  try {
    gl = canvas.getContext('webgl2');
    if (!gl) {
      report.gl = null;
    } else {
      const debugInfo = gl.getExtension('WEBGL_debug_renderer_info');
      report.gl = debugInfo ? gl.getParameter(debugInfo.UNMASKED_RENDERER_WEBGL) : gl.getParameter(gl.RENDERER);
      const compile = (type, source) => {
        const shader = gl.createShader(type);
        gl.shaderSource(shader, source);
        gl.compileShader(shader);
        return shader;
      };
      const program = gl.createProgram();
      gl.attachShader(program, compile(gl.VERTEX_SHADER, '#version 300 es\nvoid main(){vec2 p=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2));gl_Position=vec4(p*2.0-1.0,0.0,1.0);}'));
      gl.attachShader(program, compile(gl.FRAGMENT_SHADER, '#version 300 es\nprecision mediump float;out vec4 c;void main(){c=vec4(0.0,1.0,0.0,1.0);}'));
      gl.linkProgram(program);
      gl.useProgram(program);
      gl.viewport(0, 0, 8, 8);
      gl.clearColor(1, 0, 0, 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      const pixel = new Uint8Array(4);
      gl.readPixels(4, 4, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, pixel);
      report.glDraw = pixel[1] === 255 && pixel[0] === 0;
    }
  } catch (e) {
    report.glError = String(e && e.message || e);
  }
  try {
    if (!navigator.gpu) {
      report.gpu = null;
    } else {
      const adapter = await within(navigator.gpu.requestAdapter(), 5000);
      if (!adapter) {
        report.gpu = null;
      } else {
        const info = adapter.info || (adapter.requestAdapterInfo ? await adapter.requestAdapterInfo() : {});
        report.gpu = { vendor: info.vendor, arch: info.architecture, device: info.device, desc: info.description, fallback: adapter.isFallbackAdapter === true || info.isFallbackAdapter === true };
        const device = await within(adapter.requestDevice(), 5000);
        const storage = device.createBuffer({ size: 16, usage: GPUBufferUsage.STORAGE | GPUBufferUsage.COPY_SRC });
        const readback = device.createBuffer({ size: 16, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ });
        const pipeline = device.createComputePipeline({
          layout: 'auto',
          compute: {
            module: device.createShaderModule({ code: '@group(0) @binding(0) var<storage,read_write> o:array<u32>;@compute @workgroup_size(4) fn main(@builtin(global_invocation_id) g:vec3<u32>){o[g.x]=g.x*2u+1u;}' }),
            entryPoint: 'main',
          },
        });
        const bind = device.createBindGroup({ layout: pipeline.getBindGroupLayout(0), entries: [{ binding: 0, resource: { buffer: storage } }] });
        const encoder = device.createCommandEncoder();
        const pass = encoder.beginComputePass();
        pass.setPipeline(pipeline);
        pass.setBindGroup(0, bind);
        pass.dispatchWorkgroups(1);
        pass.end();
        encoder.copyBufferToBuffer(storage, 0, readback, 0, 16);
        device.queue.submit([encoder.finish()]);
        await within(readback.mapAsync(GPUMapMode.READ), 5000);
        report.gpuCompute = Array.from(new Uint32Array(readback.getMappedRange())).join(',') === '1,3,5,7';
        device.destroy();
      }
    }
  } catch (e) {
    report.gpuError = String(e && e.message || e);
  }
  const frames = await new Promise((resolve) => {
    let count = 0;
    const start = performance.now();
    const tick = (now) => {
      count += 1;
      if (gl) {
        gl.clearColor(count & 1, 0, 0, 1);
        gl.clear(gl.COLOR_BUFFER_BIT);
      }
      if (now - start >= 700) resolve({ count, ms: now - start });
      else requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
    setTimeout(() => resolve({ count, ms: performance.now() - start }), 3000);
  });
  report.fps = Math.round((frames.count / frames.ms) * 1000);
  report.visibility = document.visibilityState;
  report.focused = document.hasFocus();
  return report;
})()
