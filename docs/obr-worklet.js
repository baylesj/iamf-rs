// Pure-Rust Open Binaural Renderer (OBR) AudioWorkletProcessor.
// Consumes N-channel (e.g. 12-channel 7.1.4, 10-channel 5.1.4, 8-channel 7.1,
// 6-channel 5.1, or 2-channel Stereo) audio from a MediaElementAudioSourceNode
// and renders 2-channel binaural audio via `iamf-obr-wasm` (`obr_binaural.wasm`).

const LAYOUT_NAMES = {
  1: "Mono (1.0)",
  2: "Stereo (2.0)",
  6: "5.1 Surround",
  8: "7.1 Surround",
  10: "5.1.4 Immersive",
  12: "7.1.4 Immersive",
  16: "9.1.6 Immersive",
  17: "7.1.5.4 Immersive",
  24: "22.2 (10.2.9.3)"
};

class ObrBinauralProcessor extends AudioWorkletProcessor {
  constructor(options) {
    super();
    const opts = (options && options.processorOptions) || {};
    this.profile = typeof opts.profile === "number" ? opts.profile : 0;
    this.bypass = Boolean(opts.bypass);
    this.wasm = null;
    this.rendererPtr = 0;
    this.rendererChannels = 0;
    this.rendererProfile = -1;
    this.rendererFrameSize = 0;
    this.maxChannels = 24;
    this.inPtr = 0;
    this.outLPtr = 0;
    this.outRPtr = 0;
    this.frameCounter = 0;

    if (opts.wasmBytes) {
      this.initWasm(opts.wasmBytes);
    }

    this.port.onmessage = (event) => {
      const data = event.data || {};
      if (data.type === "wasm" && data.wasmBytes) {
        this.initWasm(data.wasmBytes);
      } else if (data.type === "config") {
        if (typeof data.profile === "number" && data.profile !== this.profile) {
          this.profile = data.profile;
          this.destroyRenderer();
        }
        if (typeof data.bypass === "boolean") {
          this.bypass = data.bypass;
        }
      }
    };
  }

  initWasm(wasmBytes) {
    try {
      const mod = new WebAssembly.Module(wasmBytes);
      const inst = new WebAssembly.Instance(mod, {});
      this.wasm = inst.exports;
      this.inPtr = this.wasm.obr_wasm_alloc_f32(this.maxChannels * 128);
      this.outLPtr = this.wasm.obr_wasm_alloc_f32(128);
      this.outRPtr = this.wasm.obr_wasm_alloc_f32(128);
      this.port.postMessage({
        type: "ready",
        sampleRate: sampleRate
      });
    } catch (err) {
      this.port.postMessage({
        type: "error",
        message: err && err.message ? err.message : String(err)
      });
    }
  }

  destroyRenderer() {
    if (this.wasm && this.rendererPtr) {
      this.wasm.obr_renderer_destroy(this.rendererPtr);
      this.rendererPtr = 0;
      this.rendererChannels = 0;
    }
  }

  ensureRenderer(numChannels, frameSize) {
    if (!this.wasm) return false;
    if (
      this.rendererPtr &&
      this.rendererChannels === numChannels &&
      this.rendererProfile === this.profile &&
      this.rendererFrameSize === frameSize
    ) {
      return true;
    }
    this.destroyRenderer();
    const ptr = this.wasm.obr_renderer_create_for_channels(
      numChannels,
      this.profile,
      frameSize,
      sampleRate,
      1 // Chromium IamfAudioDecoder uses ChannelOrdering::kOrderingForAndroid
    );
    if (!ptr) {
      return false;
    }
    this.rendererPtr = ptr;
    this.rendererChannels = numChannels;
    this.rendererProfile = this.profile;
    this.rendererFrameSize = frameSize;
    this.port.postMessage({
      type: "renderer-configured",
      inputChannels: numChannels,
      layoutName: LAYOUT_NAMES[numChannels] || `${numChannels}ch Discrete`,
      profile: this.profile,
      sampleRate: sampleRate
    });
    return true;
  }

  process(inputs, outputs) {
    const input = inputs[0];
    const output = outputs[0];
    if (!output || output.length === 0) {
      return true;
    }
    const outL = output[0];
    const outR = output.length > 1 ? output[1] : output[0];
    const frameSize = outL.length;

    if (!input || input.length === 0 || !input[0] || input[0].length === 0) {
      outL.fill(0);
      if (outR !== outL) outR.fill(0);
      return true;
    }

    const numChannels = Math.min(input.length, this.maxChannels);
    let renderedWithObr = false;

    if (!this.bypass && this.wasm && frameSize === 128 && this.ensureRenderer(numChannels, frameSize)) {
      const f32 = new Float32Array(this.wasm.memory.buffer);
      const baseIdx = this.inPtr >> 2;
      for (let ch = 0; ch < numChannels; ch++) {
        const chData = input[ch];
        if (chData && chData.length === frameSize) {
          f32.set(chData, baseIdx + ch * frameSize);
        } else {
          f32.fill(0, baseIdx + ch * frameSize, baseIdx + (ch + 1) * frameSize);
        }
      }
      const rc = this.wasm.obr_renderer_process_planar(
        this.rendererPtr,
        this.inPtr,
        numChannels,
        frameSize,
        this.outLPtr,
        this.outRPtr
      );
      if (rc === 0) {
        const f32Out = new Float32Array(this.wasm.memory.buffer);
        const lIdx = this.outLPtr >> 2;
        const rIdx = this.outRPtr >> 2;
        outL.set(f32Out.subarray(lIdx, lIdx + frameSize));
        outR.set(f32Out.subarray(rIdx, rIdx + frameSize));
        renderedWithObr = true;
      }
    }

    if (!renderedWithObr) {
      outL.set(input[0]);
      if (outR !== outL) {
        outR.set(input[numChannels > 1 ? 1 : 0]);
      }
    }

    this.frameCounter++;
    if (this.frameCounter % 3 === 0) {
      const inputRms = new Array(numChannels);
      for (let ch = 0; ch < numChannels; ch++) {
        const plane = input[ch];
        let sum = 0;
        if (plane) {
          for (let i = 0; i < plane.length; i++) {
            sum += plane[i] * plane[i];
          }
          inputRms[ch] = Math.sqrt(sum / plane.length);
        } else {
          inputRms[ch] = 0;
        }
      }
      let sumL = 0;
      let sumR = 0;
      for (let i = 0; i < frameSize; i++) {
        sumL += outL[i] * outL[i];
        sumR += outR[i] * outR[i];
      }
      this.port.postMessage({
        type: "meters",
        inputChannels: numChannels,
        layoutName: LAYOUT_NAMES[numChannels] || `${numChannels}ch`,
        renderedWithObr,
        profile: this.profile,
        inputRms,
        outputRms: [Math.sqrt(sumL / frameSize), Math.sqrt(sumR / frameSize)]
      });
    }

    return true;
  }
}

registerProcessor("obr-binaural-processor", ObrBinauralProcessor);
