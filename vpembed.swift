// vpembed: speaker embeddings with MacWhisper's own pyannote (WeSpeaker) Core ML embedder.
//
// Usage: vpembed <audio file> <segments.json> <embedder model dir>
//   segments.json: {"<speaker key>": [[startSec, endSec], ...], ...}
// Output (stdout, JSON): {"<speaker key>": {"seconds": s, "windows": [{"t": winStart, "w": activeSec, "e": [256 floats]}]}}
//
// The audio is cut into 30 s windows (what the model takes). In each window, every speaker gets a
// frame mask of where they talk; the model returns one 256-d embedding per mask row. Callers
// average the windows, weighted by active seconds. This reproduces MacWhisper's stored voiceprints
// (cosine 0.995-0.998 on calls checked on 2026-11-23).
import AVFoundation
import CoreML
import Foundation

let sampleRate = 16000.0
let windowSamples = 480_000          // 30 s
let maskFrames = 1767                // embedder frame grid per 30 s window
let maxRows = 64                     // mask rows per call
let minActiveSeconds = 1.0           // ignore a speaker in a window with less speech than this

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(("vpembed: " + message + "\n").data(using: .utf8)!)
    exit(1)
}

func loadAudio(_ path: String) -> [Float] {
    let file: AVAudioFile
    do { file = try AVAudioFile(forReading: URL(fileURLWithPath: path)) } catch { fail("cannot open audio \(path): \(error)") }
    let inFormat = file.processingFormat
    let outFormat = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: sampleRate, channels: 1, interleaved: false)!
    guard let converter = AVAudioConverter(from: inFormat, to: outFormat) else { fail("no audio converter for \(inFormat)") }
    var samples = [Float]()
    samples.reserveCapacity(Int(Double(file.length) * sampleRate / inFormat.sampleRate) + 4096)
    let inBuffer = AVAudioPCMBuffer(pcmFormat: inFormat, frameCapacity: 1 << 16)!
    var finished = false
    while true {
        let outBuffer = AVAudioPCMBuffer(pcmFormat: outFormat, frameCapacity: 1 << 16)!
        var error: NSError?
        let status = converter.convert(to: outBuffer, error: &error) { _, inputStatus in
            if finished { inputStatus.pointee = .endOfStream; return nil }
            do { try file.read(into: inBuffer) } catch { finished = true; inputStatus.pointee = .endOfStream; return nil }
            if inBuffer.frameLength == 0 { finished = true; inputStatus.pointee = .endOfStream; return nil }
            inputStatus.pointee = .haveData
            return inBuffer
        }
        if let error { fail("audio conversion failed: \(error)") }
        let n = Int(outBuffer.frameLength)
        if n > 0 { samples.append(contentsOf: UnsafeBufferPointer(start: outBuffer.floatChannelData![0], count: n)) }
        if status == .endOfStream || status == .error || (n == 0 && finished) { break }
    }
    return samples
}

let args = CommandLine.arguments
guard args.count == 4 else { fail("usage: vpembed <audio> <segments.json> <embedder model dir>") }

guard let segmentData = FileManager.default.contents(atPath: args[2]),
      let segments = (try? JSONSerialization.jsonObject(with: segmentData)) as? [String: [[Double]]]
else { fail("cannot read segments json \(args[2])") }
let speakers = segments.keys.sorted()
guard speakers.count <= maxRows else { fail("more than \(maxRows) speakers") }

let config = MLModelConfiguration()
config.computeUnits = .cpuAndNeuralEngine
let modelDir = URL(fileURLWithPath: args[3])
let preprocessor: MLModel
let embedder: MLModel
do {
    preprocessor = try MLModel(contentsOf: modelDir.appendingPathComponent("SpeakerEmbedderPreprocessor.mlmodelc"), configuration: config)
    embedder = try MLModel(contentsOf: modelDir.appendingPathComponent("SpeakerEmbedder.mlmodelc"), configuration: config)
} catch { fail("cannot load models from \(modelDir.path): \(error)") }

let audio = loadAudio(args[1])
if audio.isEmpty { fail("no audio samples in \(args[1])") }

let frameSeconds = 30.0 / Double(maskFrames)
var output: [String: [String: Any]] = [:]
for s in speakers { output[s] = ["seconds": 0.0, "windows": [[String: Any]]()] }

let windowCount = (audio.count + windowSamples - 1) / windowSamples
for w in 0..<windowCount {
    let t0 = Double(w) * 30.0
    var rows = [[Float16]](repeating: [Float16](repeating: 0, count: maskFrames), count: speakers.count)
    var active = [Double](repeating: 0, count: speakers.count)
    for (k, speaker) in speakers.enumerated() {
        var count = 0
        for seg in segments[speaker]! where seg.count == 2 {
            let a = max(seg[0], t0), b = min(seg[1], t0 + 30.0)
            if b <= a { continue }
            let fa = max(0, Int((a - t0) / frameSeconds))
            let fb = min(maskFrames, Int(ceil((b - t0) / frameSeconds)))
            if fb > fa { for f in fa..<fb where rows[k][f] == 0 { rows[k][f] = 1; count += 1 } }
        }
        active[k] = Double(count) * frameSeconds
    }
    if active.allSatisfy({ $0 < minActiveSeconds }) { continue }

    do {
        let wave = try MLMultiArray(shape: [1, NSNumber(value: windowSamples)], dataType: .float16)
        let wp = wave.dataPointer.bindMemory(to: Float16.self, capacity: windowSamples)
        let start = w * windowSamples
        for i in 0..<windowSamples { let j = start + i; wp[i] = Float16(j < audio.count ? audio[j] : 0) }
        let features = try preprocessor.prediction(from: MLDictionaryFeatureProvider(dictionary: ["waveforms": wave]))
            .featureValue(for: "preprocessor_output_1")!.multiArrayValue!

        let masks = try MLMultiArray(shape: [1, NSNumber(value: maxRows), NSNumber(value: maskFrames)], dataType: .float16)
        let mp = masks.dataPointer.bindMemory(to: Float16.self, capacity: maxRows * maskFrames)
        mp.initialize(repeating: 0, count: maxRows * maskFrames)
        for k in 0..<speakers.count { for f in 0..<maskFrames { mp[k * maskFrames + f] = rows[k][f] } }

        let embeddings = try embedder.prediction(from: MLDictionaryFeatureProvider(dictionary: [
            "preprocessor_output_1": features, "speaker_masks": masks,
        ])).featureValue(for: "speaker_embeddings")!.multiArrayValue!

        for (k, speaker) in speakers.enumerated() where active[k] >= minActiveSeconds {
            var vector = [Double](repeating: 0, count: 256)
            for d in 0..<256 { vector[d] = embeddings[[0, NSNumber(value: k), NSNumber(value: d)]].doubleValue }
            if vector.contains(where: { !$0.isFinite }) { continue }
            var entry = output[speaker]!
            entry["seconds"] = (entry["seconds"] as! Double) + active[k]
            var windows = entry["windows"] as! [[String: Any]]
            windows.append(["t": t0, "w": active[k], "e": vector])
            entry["windows"] = windows
            output[speaker] = entry
        }
    } catch {
        fail("model prediction failed in window at \(t0) s: \(error)")
    }
}

do {
    FileHandle.standardOutput.write(try JSONSerialization.data(withJSONObject: output))
} catch { fail("cannot encode output: \(error)") }
