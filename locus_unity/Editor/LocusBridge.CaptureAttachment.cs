using System;
using System.IO;
using System.IO.Compression;
using System.Linq;
using System.Threading.Tasks;
using UnityEditor;
using UnityEngine;
using UnityEngine.Rendering;

namespace Locus
{
    public static partial class LocusBridge
    {
        [Serializable] private class AttachmentCaptureRequest { public string id, target; }
        [Serializable] private class AttachmentViewContext {
            public string target, scene, projection, spatialError;
            public bool hasCamera;
            public float[] cameraPosition = new float[3], cameraForward = new float[3];
        }
        [Serializable] private class AttachmentCaptureWire {
            public int width, height;
            public string pixels;
            public SpatialCaptureWire[] snapshots;
            public AttachmentViewContext context;
            public double captureMs;
        }
        private static async Task<PipeEnvelope> HandleCaptureAttachment(string requestId, string message)
        {
            var completion = LocusAsync.CreateTcs<PipeEnvelope>();
            // Continue on Locus' editor pump even while the Player loop is paused.
            PostToMainThread(() => new ExecuteCodeSynchronizationContext().Run(async () => {
                var watch = System.Diagnostics.Stopwatch.StartNew();
                AttachmentCaptureRequest request = null;
                bool ownsLease = false;
                try {
                    request = JsonUtility.FromJson<AttachmentCaptureRequest>(message);
                    Guid captureId;
                    if (request == null || !Guid.TryParse(request.id, out captureId)
                        || (request.target != "scene" && request.target != "game"))
                        throw new InvalidOperationException("Invalid screenshot request");
                    string target, title;
                    var window = ResolveCaptureWindow(new CaptureViewportRequest { target = request.target }, out target, out title);
                    if (spatialCaptureLease != null) throw new InvalidOperationException("Another screenshot is in progress");
                    spatialCaptureLease = request.id;
                    ownsLease = true;
                    spatialCaptureExpiry = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() + 8000;
                    EditorApplication.update += CheckSpatialCaptureLease;
                    AssemblyReloadEvents.beforeAssemblyReload += ReleaseSpatialCaptureLease;
                    EditorApplication.quitting += ReleaseSpatialCaptureLease;
                    if (EditorApplication.isPlaying && !EditorApplication.isPaused) {
                        spatialCapturePausedByUs = true; EditorApplication.isPaused = true;
                    }
                    var operation = new CaptureViewportOperation { window = window };
                    spatialCaptureRestoreView = () => RestoreCaptureWindowSelection(operation);
                    PrepareCaptureWindowForCapture(operation);
                    // Refresh a hidden/docked view without moving desktop focus.
                    await SpatialCaptureNextUpdate();
                    await SpatialCaptureNextUpdate();
                    if (spatialCaptureLease != request.id) throw new InvalidOperationException("Screenshot capture timed out");
                    Camera camera = null;
                    if (window is SceneView scene) camera = scene.camera;
                    else {
                        var displayProperty = window.GetType().GetProperty("targetDisplay",
                            System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.NonPublic);
                        int display = displayProperty?.GetValue(window) is int value ? value : -1;
                        var cameras = Camera.allCameras.Where(c => c.enabled && !c.stereoEnabled && c.targetTexture == null && c.targetDisplay == display).ToArray();
                        if (cameras.Length == 1 && cameras[0].rect == new Rect(0, 0, 1, 1)) camera = cameras[0];
                    }
                    // Read only the requested viewport while its scene state is frozen.
                    var activeScene = UnityEngine.SceneManagement.SceneManager.GetActiveScene();
                    var context = new AttachmentViewContext {
                        target = target, scene = string.IsNullOrEmpty(activeScene.path) ? activeScene.name : activeScene.path,
                        hasCamera = camera != null,
                        projection = camera == null ? "unavailable" : camera.orthographic ? "orthographic" : "perspective",
                        cameraPosition = camera == null ? new float[3] : SpatialVector(camera.cameraToWorldMatrix.MultiplyPoint(Vector3.zero)),
                        cameraForward = camera == null ? new float[3] : SpatialVector(camera.cameraToWorldMatrix.MultiplyVector(Vector3.back).normalized)
                    };
                    var colorTask = ReadAttachmentColor(window, target);
                    SpatialCaptureWire spatial = null;
                    if (camera != null && SpatialCaptureSupported && SystemInfo.supportsAsyncGPUReadback &&
                        (SystemInfo.graphicsDeviceType == GraphicsDeviceType.Direct3D11 || SystemInfo.graphicsDeviceType == GraphicsDeviceType.Direct3D12)) {
                        try { spatial = await CaptureSpatialCamera(camera, target, new RectInt(0, 0, camera.pixelWidth, camera.pixelHeight)); }
                        catch (Exception ex) {
                            context.spatialError = ex.Message;
                            Debug.LogWarning("[Locus] Screenshot has no spatial data: " + ex.Message);
                        }
                    }
                    else context.spatialError = camera == null ? "No unambiguous camera for this view"
                        : !SpatialCaptureSupported ? SpatialCaptureUnsupportedMessage
                        : !SystemInfo.supportsAsyncGPUReadback ? "GPU readback unavailable" : "Unsupported graphics API";
                    var color = await colorTask;
                    if (spatialCaptureLease != request.id) throw new InvalidOperationException("Screenshot capture timed out");
                    ReleaseSpatialCaptureLease();
                    double gpuMs = watch.Elapsed.TotalMilliseconds;
                    string pixels = await Task.Run(() => CompressAttachmentPixels(color.raw, color.width, color.height, color.channels));
                    Debug.Log("[LocusCapture] " + target + " gpu=" + gpuMs.ToString("F1") + "ms total=" + watch.Elapsed.TotalMilliseconds.ToString("F1") + "ms");
                    completion.TrySetResult(OkResponse(requestId, JsonUtility.ToJson(new AttachmentCaptureWire {
                        width = color.width, height = color.height, pixels = pixels, context = context,
                        snapshots = spatial == null ? new SpatialCaptureWire[0] : new[] { spatial }, captureMs = watch.Elapsed.TotalMilliseconds
                    })));
                } catch (Exception ex) { completion.TrySetResult(ErrorResponse(requestId, ex.Message)); }
                finally {
                    if (ownsLease && spatialCaptureLease == request.id) ReleaseSpatialCaptureLease();
                }
            }));
            return await LocusAsync.WithTimeout(completion.Task, 10000, "capture_attachment");
        }
        private sealed class AttachmentColor { public int width, height, channels; public byte[] raw; }
        private static async Task<AttachmentColor> ReadAttachmentColor(EditorWindow window, string target)
        {
            RenderTexture source = null;
            if (target == "game") {
                EnsureCaptureGameViewTextureField(window);
                source = s_CaptureGameViewTextureField.GetValue(window) as RenderTexture;
            } else {
                var field = typeof(SceneView).GetField("m_LDRSceneTargetTexture", System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic);
                source = field?.GetValue(window) as RenderTexture;
            }
            if (source == null || !source.IsCreated())
                throw new InvalidOperationException("Unity view has no rendered image. Open the view and try again.");
            int width, height; CalculateCaptureOutputSize(source.width, source.height, 1280, out width, out height);
            var resized = RenderTexture.GetTemporary(width, height, 0, RenderTextureFormat.ARGB32, RenderTextureReadWrite.Default, 1);
            try {
                // Game and Scene render targets have different D3D orientations.
                if (target == "game" && SystemInfo.graphicsUVStartsAtTop) Graphics.Blit(source, resized, new Vector2(1, -1), new Vector2(0, 1));
                else Graphics.Blit(source, resized);
                if (!SystemInfo.supportsAsyncGPUReadback) {
                    var texture = new Texture2D(width, height, TextureFormat.RGBA32, false);
                    var previous = RenderTexture.active;
                    try {
                        RenderTexture.active = resized;
                        texture.ReadPixels(new Rect(0, 0, width, height), 0, 0);
                        texture.Apply(false, false);
                        return new AttachmentColor { width = width, height = height, channels = 4,
                            raw = texture.GetRawTextureData<byte>().ToArray() };
                    } finally { RenderTexture.active = previous; UnityEngine.Object.DestroyImmediate(texture); }
                }
                return new AttachmentColor { width = width, height = height, channels = 4,
                    raw = await SpatialReadback<byte>(resized, TextureFormat.RGBA32) };
            } finally { RenderTexture.ReleaseTemporary(resized); }
        }
        private static string CompressAttachmentPixels(byte[] source, int width, int height, int channels)
        {
            var rgba = new byte[width * height * 4];
            for (int y = 0; y < height; y++) for (int x = 0; x < width; x++) {
                int src = ((height - y - 1) * width + x) * channels, dst = (y * width + x) * 4;
                rgba[dst] = source[src]; rgba[dst + 1] = source[src + 1]; rgba[dst + 2] = source[src + 2]; rgba[dst + 3] = 255;
            }
            using (var stream = new MemoryStream()) {
                using (var zip = new DeflateStream(stream, System.IO.Compression.CompressionLevel.Fastest, true)) zip.Write(rgba, 0, rgba.Length);
                return Convert.ToBase64String(stream.ToArray());
            }
        }
    }
}
