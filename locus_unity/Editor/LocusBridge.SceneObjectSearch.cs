using System;
using System.Collections.Generic;
using System.Text;
using System.Threading.Tasks;
using UnityEditor;
using UnityEditor.SceneManagement;
using UnityEngine;

namespace Locus
{
    public static partial class LocusBridge
    {
        [Serializable]
        private sealed class SceneObjectSearchRequest
        {
            public string query;
            public int limit = 160;
        }

        private static async Task<PipeEnvelope> HandleSceneObjectSearch(string reqId, string message)
        {
            try
            {
                var entries = LocusSceneObjectSearch.CachedIndex;
                if (entries == null)
                {
                    var pending = LocusAsync.CreateTcs<LocusSceneObjectSearch.Entry[]>();
                    PostToMainThread(delegate
                    {
                        try { pending.SetResult(LocusSceneObjectSearch.GetIndex()); }
                        catch (Exception error) { pending.SetException(error); }
                    });
                    entries = await pending.Task.ConfigureAwait(false);
                }
                // No Unity objects leave the main thread. Warm searches only
                // capture the immutable index; ranking/JSON run on a worker.
                return await Task.Run(() =>
                {
                    var request = JsonUtility.FromJson<SceneObjectSearchRequest>(message)
                        ?? new SceneObjectSearchRequest();
                    var results = LocusSceneObjectSearch.Search(entries, request.query, request.limit);
                    return OkResponse(reqId, JsonUtility.ToJson(results));
                }).ConfigureAwait(false);
            }
            catch (Exception error) { return ErrorResponse(reqId, error.Message); }
        }
    }

    // Register invalidation on the main thread at domain load. Building is
    // lazy; warm requests can read the snapshot without waiting for an editor tick.
    [InitializeOnLoad]
    internal static class LocusSceneObjectSearch
    {
        internal sealed class Entry
        {
            internal readonly string scenePath, objectPath, name;
            internal readonly string lowerName, lowerPath, compactName, compactPath;
            internal readonly int depthPenalty;
            internal readonly ulong characters;

            internal Entry(string scene, string path, string objectName, int depth)
            {
                scenePath = scene; objectPath = path; name = objectName;
                lowerName = objectName.ToLowerInvariant();
                lowerPath = (scene + "/" + path).ToLowerInvariant();
                compactName = Compact(lowerName); compactPath = Compact(lowerPath);
                characters = CharacterMask(compactPath);
                depthPenalty = Math.Min(depth, 80) * 2;
            }
        }

        [Serializable]
        internal sealed class Result
        {
            public string scenePath, objectPath, name;
            public int matchScore;
        }

        [Serializable]
        internal sealed class Response
        {
            public Result[] results;
        }

        private struct PendingNode
        {
            internal Transform transform;
            internal string path;
            internal string name;
            internal int depth;
        }

        private struct Match
        {
            internal Entry entry;
            internal int score;
        }

        private static Entry[] _entries;
        private static string _sceneSignature;
        private static volatile bool _dirty = true;
        private static readonly object SearchCacheLock = new object();
        private static readonly Dictionary<string, Response> SearchCache = new Dictionary<string, Response>();
        private static readonly Queue<string> SearchCacheOrder = new Queue<string>();
        private static Entry[] _searchCacheIndex;

        internal static Entry[] CachedIndex => _dirty ? null : _entries;

        static LocusSceneObjectSearch()
        {
            EditorApplication.hierarchyChanged += Invalidate;
            ObjectChangeEvents.changesPublished += OnObjectChanges;
            Undo.undoRedoPerformed += Invalidate;
            EditorSceneManager.sceneOpened += (_, __) => Invalidate();
            EditorSceneManager.sceneClosed += _ => Invalidate();
            EditorSceneManager.sceneSaved += _ => Invalidate();
            EditorApplication.playModeStateChanged += _ => Invalidate();
        }

        private static void Invalidate() { _dirty = true; }

        private static void OnObjectChanges(ref ObjectChangeEventStream changes)
        {
            if (_dirty) return;
            for (int i = 0; i < changes.length; ++i)
            {
                switch (changes.GetEventType(i))
                {
                    case ObjectChangeKind.ChangeScene:
                    case ObjectChangeKind.CreateGameObjectHierarchy:
                    case ObjectChangeKind.DestroyGameObjectHierarchy:
                    case ObjectChangeKind.ChangeGameObjectStructureHierarchy:
                    case ObjectChangeKind.ChangeGameObjectParent:
#if UNITY_2022_2_OR_NEWER
                    // Older editors report sibling reorders only via hierarchyChanged.
                    case ObjectChangeKind.ChangeChildrenOrder:
#endif
                    case ObjectChangeKind.UpdatePrefabInstances:
                        Invalidate(); return;
                    case ObjectChangeKind.ChangeGameObjectOrComponentProperties:
                        changes.GetChangeGameObjectOrComponentPropertiesEvent(i, out var change);
#if UNITY_6000_5_OR_NEWER
                        var target = EditorUtility.EntityIdToObject(change.entityId);
#else
                        var target = EditorUtility.InstanceIDToObject(change.instanceId);
#endif
                        // Names/hide flags belong to GameObjects. Component
                        // values and moving Transforms do not change this index.
                        if (target is GameObject) { Invalidate(); return; }
                        break;
                }
            }
        }

        internal static Entry[] GetIndex()
        {
            // Covers scene load/unload, Save As, and domain-reload-disabled
            // play transitions even before the delayed hierarchy event arrives.
            var scenes = new List<UnityEngine.SceneManagement.Scene>();
            var signature = new StringBuilder();
            for (int i = 0; i < EditorSceneManager.sceneCount; ++i)
            {
                var scene = EditorSceneManager.GetSceneAt(i);
                if (!scene.IsValid() || !scene.isLoaded || EditorSceneManager.IsPreviewScene(scene)
                    || !scene.path.EndsWith(".unity", StringComparison.OrdinalIgnoreCase)) continue;
                // Open ECS SubScenes are ordinary loaded authoring scenes here.
                // Do not filter scene.isSubScene or walk only the active scene.
                scenes.Add(scene);
                signature.Append(scene.handle.ToString()).Append(':').Append(scene.path)
                    .Append(':').Append(scene.rootCount).Append('\n');
            }
            string currentSignature = signature.ToString();
            if (!_dirty && _entries != null && _sceneSignature == currentSignature) return _entries;

            var entries = new List<Entry>();
            var stack = new Stack<PendingNode>();
            var siblings = new List<Transform>();
            var siblingNames = new List<string>();
            var totals = new Dictionary<string, int>(StringComparer.Ordinal);
            var ordinals = new Dictionary<string, int>(StringComparer.Ordinal);
            foreach (var scene in scenes)
            {
                string scenePath = scene.path;
                siblings.Clear();
                foreach (var root in scene.GetRootGameObjects()) siblings.Add(root.transform);
                int sceneDepth = 0;
                foreach (char c in scenePath) if (c == '/') ++sceneDepth;
                PushSiblings(stack, siblings, siblingNames, "", sceneDepth + 1, totals, ordinals);
                while (stack.Count > 0)
                {
                    var node = stack.Pop();
                    var transform = node.transform;
                    // Inactive authoring objects remain searchable. Hidden
                    // editor helpers/previews must not leak into the picker.
                    if (transform == null || (transform.gameObject.hideFlags & HideFlags.HideInHierarchy) != 0) continue;
                    entries.Add(new Entry(scenePath, node.path, node.name, node.depth));
                    int childCount = transform.childCount;
                    if (childCount == 0) continue;
                    siblings.Clear();
                    for (int i = 0; i < childCount; ++i) siblings.Add(transform.GetChild(i));
                    PushSiblings(stack, siblings, siblingNames, node.path, node.depth + 1, totals, ordinals);
                }
            }
            _sceneSignature = currentSignature;
            _entries = entries.ToArray();
            _dirty = false;
            return _entries;
        }

        private static void PushSiblings(Stack<PendingNode> stack, List<Transform> siblings, List<string> names,
            string parent, int depth, Dictionary<string, int> totals, Dictionary<string, int> ordinals)
        {
            totals.Clear(); ordinals.Clear(); names.Clear();
            foreach (var sibling in siblings)
            {
                string name = sibling.name;
                names.Add(name);
                totals.TryGetValue(name, out int count);
                totals[name] = count + 1;
            }
            // Reverse traversal preserves Unity's sibling order when popped.
            // Count hidden siblings too: the existing scene-object resolver does.
            for (int i = siblings.Count - 1; i >= 0; --i)
            {
                var transform = siblings[i];
                string name = names[i];
                ordinals.TryGetValue(name, out int fromEnd);
                ordinals[name] = fromEnd + 1;
                string segment = totals[name] > 1 ? name + "[" + (totals[name] - fromEnd) + "]" : name;
                stack.Push(new PendingNode {
                    transform = transform, name = name, path = parent.Length == 0 ? segment : parent + "/" + segment, depth = depth
                });
            }
        }

        internal static Response Search(Entry[] entries, string query, int requestedLimit)
        {
            int limit = Math.Max(1, Math.Min(requestedLimit, 500));
            string lowerQuery = (query ?? "").Trim().ToLowerInvariant();
            string cacheKey = limit + "\0" + lowerQuery;
            lock (SearchCacheLock)
            {
                if (ReferenceEquals(_searchCacheIndex, entries) && SearchCache.TryGetValue(cacheKey, out var cached)) return cached;
            }
            string compactQuery = Compact(lowerQuery);
            ulong characters = CharacterMask(compactQuery);
            var best = new List<Match>(limit + 1);
            foreach (var entry in entries)
            {
                int threshold = best.Count == limit ? best[best.Count - 1].score : int.MinValue;
                int score = Score(entry, lowerQuery, compactQuery, characters, threshold);
                if (score == int.MinValue || (best.Count == limit && score <= best[best.Count - 1].score)) continue;
                // Bounded top-k; never sort/materialize every matching object.
                int low = 0, high = best.Count;
                while (low < high)
                {
                    int mid = (low + high) / 2;
                    if (best[mid].score >= score) low = mid + 1;
                    else high = mid;
                }
                best.Insert(low, new Match { entry = entry, score = score });
                if (best.Count > limit) best.RemoveAt(limit);
                if (lowerQuery.Length == 0 && best.Count == limit) break;
            }
            var results = new Result[best.Count];
            for (int i = 0; i < best.Count; ++i)
            {
                var match = best[i];
                results[i] = new Result { scenePath = match.entry.scenePath, objectPath = match.entry.objectPath,
                    name = match.entry.name, matchScore = match.score };
            }
            var response = new Response { results = results };
            lock (SearchCacheLock)
            {
                if (!ReferenceEquals(_searchCacheIndex, entries))
                {
                    SearchCache.Clear(); SearchCacheOrder.Clear(); _searchCacheIndex = entries;
                }
                if (!SearchCache.ContainsKey(cacheKey))
                {
                    if (SearchCache.Count >= 16) SearchCache.Remove(SearchCacheOrder.Dequeue());
                    SearchCacheOrder.Enqueue(cacheKey);
                    SearchCache[cacheKey] = response;
                }
            }
            return response;
        }

        private static int Score(Entry entry, string query, string compactQuery, ulong characters, int threshold)
        {
            if (query.Length == 0) return 0;
            // A compact subsequence must contain every query character. Hash
            // collisions only admit extra candidates; they never hide matches.
            if ((entry.characters & characters) != characters) return int.MinValue;
            int score, position;
            if (entry.lowerName == query) score = 1240;
            else if (entry.lowerName.StartsWith(query, StringComparison.Ordinal)) score = 1140 - Math.Min(entry.lowerName.Length, 48);
            else if ((position = entry.lowerName.IndexOf(query, StringComparison.Ordinal)) >= 0) score = 1040 - Math.Min(position, 80) * 6;
            else if (threshold >= 960 - entry.depthPenalty) return int.MinValue;
            else if ((position = entry.lowerPath.IndexOf(query, StringComparison.Ordinal)) >= 0) score = 960 - Math.Min(position, 120) * 3;
            else if (compactQuery.Length == 0) return int.MinValue;
            else if (entry.compactName.StartsWith(compactQuery, StringComparison.Ordinal)) score = 900 - Math.Min(entry.compactName.Length, 48);
            else if ((position = entry.compactPath.IndexOf(compactQuery, StringComparison.Ordinal)) >= 0) score = 820 - Math.Min(position, 120) * 2;
            else if (threshold >= 520 - entry.depthPenalty) return int.MinValue;
            else if (IsSubsequence(compactQuery, entry.compactPath)) score = 520;
            else return int.MinValue;
            return score - entry.depthPenalty;
        }

        private static string Compact(string value)
        {
            var result = new StringBuilder(value.Length);
            foreach (char c in value) if (char.IsLetterOrDigit(c)) result.Append(c);
            return result.ToString();
        }

        private static ulong CharacterMask(string value)
        {
            ulong mask = 0;
            foreach (char c in value) mask |= 1UL << (c & 63);
            return mask;
        }

        private static bool IsSubsequence(string query, string value)
        {
            int position = 0;
            foreach (char c in value)
                if (position < query.Length && c == query[position] && ++position == query.Length) return true;
            return false;
        }
    }
}
