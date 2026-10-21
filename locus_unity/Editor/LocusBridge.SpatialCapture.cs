using System;
using System.Collections.Generic;
using System.IO;
using System.IO.Compression;
using System.Linq;
using System.Threading.Tasks;
using UnityEditor;
using UnityEngine;
using UnityEngine.Experimental.Rendering;
using UnityEngine.Rendering;

namespace Locus
{
    public static partial class LocusBridge
    {
        [Serializable] private class SpatialCaptureBatch { public int processId = System.Diagnostics.Process.GetCurrentProcess().Id; public SpatialCaptureWire[] snapshots; }
        [Serializable] private class SpatialCaptureLease { public string leaseId; public long expiresAt; }
        private static string spatialCaptureLease;
        private static long spatialCaptureExpiry;
        private static bool spatialCapturePausedByUs;
        private static Action spatialCaptureRestoreView;
        // ObjectIdRequest exists in Unity 2022.3.11+ and 2023.2+, not in 2023.1.
        // LOCUS_HAS_OBJECT_ID_REQUEST comes from Locus.Editor.asmdef versionDefines.
#if UNITY_2023_2_OR_NEWER || LOCUS_HAS_OBJECT_ID_REQUEST
        private static readonly bool SpatialCaptureSupported = true;
#else
        private static readonly bool SpatialCaptureSupported = false;
#endif
        private const string SpatialCaptureUnsupportedMessage = "Spatial capture requires Unity 2022.3.11+ or 2023.2+";

        private static void ReleaseSpatialCaptureLease()
        {
            EditorApplication.update -= CheckSpatialCaptureLease;
            AssemblyReloadEvents.beforeAssemblyReload -= ReleaseSpatialCaptureLease;
            EditorApplication.quitting -= ReleaseSpatialCaptureLease;
            spatialCaptureLease = null;
            var restoreView = spatialCaptureRestoreView;
            spatialCaptureRestoreView = null;
            try { restoreView?.Invoke(); }
            catch (Exception ex) { Debug.LogWarning("[Locus] Failed to restore capture view: " + ex.Message); }
            if (spatialCapturePausedByUs && EditorApplication.isPlaying && EditorApplication.isPaused)
                EditorApplication.isPaused = false;
            spatialCapturePausedByUs = false;
        }
        private static void CheckSpatialCaptureLease()
        {
            if (DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() >= spatialCaptureExpiry)
                ReleaseSpatialCaptureLease();
        }
        private static async Task<PipeEnvelope> HandleSpatialCaptureRelease(string requestId, string message)
        {
            var completion = LocusAsync.CreateTcs<PipeEnvelope>();
            PostToMainThread(() => {
                try {
                    var lease = JsonUtility.FromJson<SpatialCaptureLease>(message);
                    if (lease != null && lease.leaseId == spatialCaptureLease) ReleaseSpatialCaptureLease();
                    completion.TrySetResult(OkResponse(requestId, "released"));
                } catch (Exception ex) { completion.TrySetResult(ErrorResponse(requestId, ex.Message)); }
            });
            return await completion.Task;
        }
        private static Task SpatialCaptureNextUpdate()
        {
            var completion = LocusAsync.CreateTcs<bool>();
            EditorApplication.CallbackFunction callback = null;
            callback = () => { EditorApplication.update -= callback; completion.TrySetResult(true); };
            EditorApplication.update += callback;
            return completion.Task;
        }
        [Serializable] private class SpatialObjectWire { public uint id; public string name, path, globalObjectId; }
        [Serializable] private class SpatialCaptureWire
        {
            public int version = 1;
            public string target, scene, projection, quality = "editor-picking-geometry";
            public int width, height;
            public float[] cameraPosition, cameraForward, inverseViewProjection, viewMatrix;
            public int[] screenRect;
            public SpatialObjectWire[] objects;
            public string payload;
        }

        private static async Task<PipeEnvelope> HandleSpatialCapture(string requestId, string message)
        {
            var completion = LocusAsync.CreateTcs<PipeEnvelope>();
            PostToMainThread(async delegate
            {
                try
                {
                    var snapshots = new List<SpatialCaptureWire>();
                    var lease = JsonUtility.FromJson<SpatialCaptureLease>(message);
                    long now = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();
                    if (lease == null || string.IsNullOrEmpty(lease.leaseId) || lease.expiresAt <= now)
                        throw new InvalidOperationException("Screenshot request expired");
                    if (spatialCaptureLease != null) throw new InvalidOperationException("Screenshot capture already active");
                    spatialCaptureLease = lease.leaseId;
                    spatialCaptureExpiry = Math.Min(lease.expiresAt, now + 8000);
                    EditorApplication.update += CheckSpatialCaptureLease;
                    AssemblyReloadEvents.beforeAssemblyReload += ReleaseSpatialCaptureLease;
                    EditorApplication.quitting += ReleaseSpatialCaptureLease;
                    if (EditorApplication.isPlaying && !EditorApplication.isPaused) {
                        spatialCapturePausedByUs = true;
                        EditorApplication.isPaused = true;
                        // Let the paused Game view reach the desktop before GPU sampling.
                        UnityEditorInternal.InternalEditorUtility.RepaintAllViews();
                        await SpatialCaptureNextUpdate();
                        await SpatialCaptureNextUpdate();
                    }
                    if (SpatialCaptureSupported && SystemInfo.supportsAsyncGPUReadback
                        && (SystemInfo.graphicsDeviceType == GraphicsDeviceType.Direct3D11 || SystemInfo.graphicsDeviceType == GraphicsDeviceType.Direct3D12))
                    {
                        foreach (string target in new[] { "scene", "game" })
                        {
                            try
                            {
                                string resolved, title;
                                var window = ResolveCaptureWindow(new CaptureViewportRequest { target = target }, out resolved, out title);
                                if (!IsCaptureWindowSelected(window)) continue;
                                Camera camera = null;
                                if (window is SceneView sceneView) camera = sceneView.camera;
                                else
                                {
                                    var cameras = Camera.allCameras.Where(c => c.enabled && c.targetTexture == null && c.targetDisplay == 0).ToArray();
                                    // Stacks/multiple cameras need an explicit compositor adapter.
                                    if (cameras.Length != 1 || cameras[0].rect != new Rect(0, 0, 1, 1)) continue;
                                    camera = cameras[0];
                                }
                                if (camera == null) continue;
                                var region = ResolveCaptureViewportRegion(window, target, false);
                                snapshots.Add(await CaptureSpatialCamera(camera, target, region.screenPixels));
                            }
                            catch (Exception ex) { Debug.LogWarning("[Locus] Spatial capture unavailable: " + ex.Message); }
                        }
                    }
                    if (spatialCaptureLease != lease.leaseId) snapshots.Clear();
                    completion.TrySetResult(OkResponse(requestId, JsonUtility.ToJson(new SpatialCaptureBatch { snapshots = snapshots.ToArray() })));
                }
                catch (Exception ex) { completion.TrySetResult(ErrorResponse(requestId, ex.Message)); }
            });
            return await LocusAsync.WithTimeout(completion.Task, 10000, "capture_spatial_snapshot");
        }

        private static float[] SpatialVector(Vector3 v) { return new[] { v.x, v.y, v.z }; }
        private static float[] SpatialMatrix(Matrix4x4 m)
        {
            var values = new float[16];
            for (int r = 0; r < 4; r++) for (int c = 0; c < 4; c++) values[r * 4 + c] = m[r, c];
            return values;
        }
        private static string SpatialObjectPath(Transform t)
        {
            var names = new List<string>();
            while (t != null) { names.Add(t.name); t = t.parent; }
            names.Reverse(); return string.Join("/", names.ToArray());
        }
#if !(UNITY_2023_2_OR_NEWER || LOCUS_HAS_OBJECT_ID_REQUEST)
        private static Task<SpatialCaptureWire> CaptureSpatialCamera(Camera source, string target, RectInt rect)
        {
            return Task.FromException<SpatialCaptureWire>(new NotSupportedException(SpatialCaptureUnsupportedMessage));
        }
#else
        private static async Task<SpatialCaptureWire> CaptureSpatialCamera(Camera source, string target, RectInt rect)
        {
            int width, height;
            CalculateCaptureOutputSize(Mathf.Max(1, source.pixelWidth), Mathf.Max(1, source.pixelHeight), 1280, out width, out height);
            var cameraObject = new GameObject("Locus spatial capture") { hideFlags = HideFlags.HideAndDontSave };
            var camera = cameraObject.AddComponent<Camera>();
            camera.CopyFrom(source); camera.enabled = false;
            camera.transform.SetPositionAndRotation(source.transform.position, source.transform.rotation);
            camera.worldToCameraMatrix = source.worldToCameraMatrix;
            camera.projectionMatrix = source.projectionMatrix;
            var ids = new RenderTexture(width, height, GraphicsFormat.R8G8B8A8_UNorm, GraphicsFormat.D32_SFloat);
            var depth = new RenderTexture(width, height, 0, RenderTextureFormat.RFloat, RenderTextureReadWrite.Linear);
            var sourcePosition = SpatialVector(source.cameraToWorldMatrix.MultiplyPoint(Vector3.zero));
            var sourceForward = SpatialVector(source.cameraToWorldMatrix.MultiplyVector(Vector3.back).normalized);
            var sourceProjection = source.orthographic ? "orthographic" : "perspective";
            var sourceScene = UnityEngine.SceneManagement.SceneManager.GetActiveScene().path;
            Material material = null;
            try
            {
                ids.antiAliasing = 1; ids.filterMode = FilterMode.Point;
                ids.Create(); depth.Create();
                camera.targetTexture = ids; camera.rect = new Rect(0, 0, 1, 1);
                var view = source.worldToCameraMatrix;
                // Native editor picking readback is bottom-up on D3D11/D3D12.
                var inverse = (GL.GetGPUProjectionMatrix(source.projectionMatrix, false) * view).inverse;
                var request = new ObjectIdRequest(ids);
                camera.SubmitRenderRequest(request);
                var shader = Resources.Load<Shader>("LocusCaptureDepth") ?? Shader.Find("Hidden/Locus/CaptureDepth");
                if (shader == null || !shader.isSupported) throw new InvalidOperationException("Depth copy shader unavailable");
                material = new Material(shader) { hideFlags = HideFlags.HideAndDontSave };
                using (var command = new CommandBuffer())
                {
                    command.SetGlobalTexture("_LocusCaptureDepth", new RenderTargetIdentifier(ids), RenderTextureSubElement.Depth);
                    command.SetRenderTarget(depth);
                    command.DrawProcedural(Matrix4x4.identity, material, 0, MeshTopology.Triangles, 3);
                    Graphics.ExecuteCommandBuffer(command);
                }
                var colorsTask = SpatialReadColors(ids);
                var depthTask = SpatialReadDepth(depth);
                await Task.WhenAll(colorsTask, depthTask);
                var colors = colorsTask.Result;
                var depths = depthTask.Result;
                var mapping = request.result.idToObjectMapping;
                var objects = new Dictionary<uint, SpatialObjectWire>();
                var indexes = new uint[colors.Length];
                var visible = new bool[mapping.Length];
                for (int i = 0; i < colors.Length; i++)
                {
                    int index = ObjectIdResult.DecodeIdFromColor(colors[i]);
                    if (index < 0 || index >= mapping.Length) continue;
                    indexes[i] = (uint)index + 1; visible[index] = true;
                }
                // Resolve each visible Unity object once, not once per pixel.
                for (int index = 0; index < mapping.Length; index++) {
                    if (!visible[index]) continue;
                    var obj = mapping[index];
                    var go = obj as GameObject ?? (obj as Component)?.gameObject;
                    if (go == null) continue;
                    uint id = (uint)index + 1;
                    if (!objects.ContainsKey(id)) objects[id] = new SpatialObjectWire {
                        id = id, name = go.name, path = SpatialObjectPath(go.transform),
                        globalObjectId = GlobalObjectId.GetGlobalObjectIdSlow(go).ToString()
                    };
                }
                string payload = await Task.Run(() => CompressSpatialCapture(indexes, depths));
                return new SpatialCaptureWire {
                    target = target, scene = sourceScene,
                    projection = sourceProjection, width = width, height = height,
                    cameraPosition = sourcePosition,
                    cameraForward = sourceForward,
                    inverseViewProjection = SpatialMatrix(inverse), viewMatrix = SpatialMatrix(view),
                    screenRect = new[] { rect.x, rect.y, rect.width, rect.height },
                    objects = objects.Values.ToArray(), payload = payload
                };
            }
            finally
            {
                UnityEngine.Object.DestroyImmediate(cameraObject);
                UnityEngine.Object.DestroyImmediate(ids); UnityEngine.Object.DestroyImmediate(depth);
                if (material != null) UnityEngine.Object.DestroyImmediate(material);
            }
        }
#endif
        private static string CompressSpatialCapture(uint[] indexes, float[] depths) {
                var raw = new byte[indexes.Length * 8];
                Buffer.BlockCopy(indexes, 0, raw, 0, indexes.Length * 4);
                Buffer.BlockCopy(depths, 0, raw, indexes.Length * 4, depths.Length * 4);
                using (var stream = new MemoryStream())
                {
                    using (var zip = new DeflateStream(stream, System.IO.Compression.CompressionLevel.Fastest, true))
                        zip.Write(raw, 0, raw.Length);
                    return Convert.ToBase64String(stream.ToArray());
                }
        }
        private static Task<Color32[]> SpatialReadColors(RenderTexture texture) => SpatialReadback<Color32>(texture);
        private static Task<float[]> SpatialReadDepth(RenderTexture texture) => SpatialReadback<float>(texture);

        // Paused players do not reliably pump AsyncGPUReadback callbacks. Update
        // only these requests from the editor loop; never advance gameplay.
        private static Task<T[]> SpatialReadback<T>(RenderTexture texture, TextureFormat? format = null) where T : struct
        {
            var task = new TaskCompletionSource<T[]>();
            var request = default(AsyncGPUReadbackRequest);
            double expires = EditorApplication.timeSinceStartup + 5;
            EditorApplication.CallbackFunction poll = null;
            poll = () => {
                if (task.Task.IsCompleted) { EditorApplication.update -= poll; return; }
                if (EditorApplication.timeSinceStartup >= expires) {
                    EditorApplication.update -= poll;
                    task.TrySetException(new TimeoutException("Screenshot GPU readback timed out"));
                    return;
                }
                request.Update();
            };
            Action<AsyncGPUReadbackRequest> receive = result => {
                EditorApplication.update -= poll;
                if (task.Task.IsCompleted) return;
                if (result.hasError) task.TrySetException(new InvalidOperationException("Screenshot GPU readback failed"));
                else task.TrySetResult(result.GetData<T>().ToArray());
            };
            request = format.HasValue ? AsyncGPUReadback.Request(texture, 0, format.Value, receive)
                : AsyncGPUReadback.Request(texture, 0, receive);
            if (!task.Task.IsCompleted) EditorApplication.update += poll;
            return task.Task;
        }
    }
}
