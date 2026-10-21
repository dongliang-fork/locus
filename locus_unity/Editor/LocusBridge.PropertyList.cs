using System;
using System.Collections;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Reflection;
using UnityEditor;

namespace Locus
{
    public static partial class LocusBridge
    {
        // Optional Odin metadata crosses the existing attribute protocol. The bridge
        // has no Odin assembly dependency and never evaluates Odin expression strings.
        private static string PropertyListAttributeValue(Attribute attribute)
        {
            string name = attribute.GetType().FullName;
            if (name == "Sirenix.OdinInspector.FoldoutGroupAttribute")
                return Convert.ToString(ReadListMember(attribute, "GroupName"), CultureInfo.InvariantCulture) ?? "";
            if (name != "Sirenix.OdinInspector.ListDrawerSettingsAttribute") return "";
            var values = new Dictionary<string, object>();
            foreach (string member in new[] { "HideAddButton", "HideRemoveButton", "ShowFoldout", "ListElementLabelName", "AddCopiesLastElement", "AlwaysAddDefaultValue" })
                values[member] = ReadListMember(attribute, member);
            string[] members = { "ShowPaging", "DraggableItems", "NumberOfItemsPerPage", "IsReadOnly", "ShowItemCount", "ShowIndexLabels", "DefaultExpandedState" };
            string[] flags = { "PagingHasValue", "DraggableHasValue", "NumberOfItemsPerPageHasValue", "IsReadOnlyHasValue", "ShowItemCountHasValue", "ShowIndexLabelsHasValue", "DefaultExpandedStateHasValue" };
            for (int i = 0; i < members.Length; i++)
                if (Equals(ReadListMember(attribute, flags[i]), true)) values[members[i]] = ReadListMember(attribute, members[i]);
            return Locus.Json.LocusJson.SerializeData(values);
        }

        private static object ReadListMember(object owner, string name)
        {
            if (owner == null || string.IsNullOrEmpty(name) || name.StartsWith("@", StringComparison.Ordinal)) return null;
            for (Type type = owner.GetType(); type != null; type = type.BaseType)
            {
                const BindingFlags flags = BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.DeclaredOnly;
                FieldInfo field = type.GetField(name, flags);
                if (field != null) return field.GetValue(owner);
                PropertyInfo property = type.GetProperty(name, flags);
                if (property != null && property.GetIndexParameters().Length == 0 && property.GetGetMethod(true) != null)
                    return property.GetValue(owner, null);
            }
            return null;
        }

        private static object ResolveListMemberObject(SerializedProperty property, string path)
        {
            object current = property.serializedObject.targetObject;
            foreach (string part in path.Replace(".Array.data[", "[").Split('.'))
            {
                if (string.IsNullOrEmpty(part)) continue;
                int bracket = part.IndexOf('[');
                string member = bracket < 0 ? part : part.Substring(0, bracket);
                if (!string.IsNullOrEmpty(member)) current = ReadListMember(current, member);
                while (bracket >= 0 && current != null)
                {
                    int end = part.IndexOf(']', bracket);
                    int index;
                    IList list = current as IList;
                    if (end < 0 || list == null || !int.TryParse(part.Substring(bracket + 1, end - bracket - 1), out index) || index < 0 || index >= list.Count) return null;
                    current = list[index];
                    bracket = part.IndexOf('[', end);
                }
                if (current == null) return null;
            }
            return current;
        }

        private static readonly HashSet<Type> PropertyTreeDynamicPresentationTypes = new HashSet<Type>();

        private static void ApplyPropertyListPresentation(SerializedProperty property, SerializedPropertySnapshot snapshot, FieldInfo field)
        {
            if (field == null) return;
            var metadata = SerializedFieldMetadataFor(field);
            if (metadata.HasDynamicListPresentation && property.serializedObject.targetObject != null)
                PropertyTreeDynamicPresentationTypes.Add(property.serializedObject.targetObject.GetType());
            if (!snapshot.isArray && !metadata.HasDynamicListPresentation) return;
            try
            {
                var attributes = new List<SerializedPropertyAttributeInfo>(snapshot.attributes ?? new SerializedPropertyAttributeInfo[0]);
                if (snapshot.isArray)
                {
                    Type elementType = SerializedElementType(field.FieldType);
                    attributes.Add(new SerializedPropertyAttributeInfo {
                        type = "Locus.ListElementType", displayName = "ListElementType",
                        value = Locus.Json.LocusJson.SerializeData(new Dictionary<string, object> {
                            { "native", true }, { "fullName", FieldTypeFullName(elementType) },
                            { "assembly", FieldTypeAssembly(elementType) },
                            { "reference", elementType != null && typeof(UnityEngine.Object).IsAssignableFrom(elementType) }
                        })
                    });
                }
                object[] declarations = metadata.Declarations;
                string path = property.propertyPath;
                bool element = path.EndsWith("]", StringComparison.Ordinal) && path.LastIndexOf(".Array.data[", StringComparison.Ordinal) >= 0;
                if (element)
                {
                    object declaration = declarations.FirstOrDefault(value => value.GetType().FullName == "Sirenix.OdinInspector.ListDrawerSettingsAttribute");
                    string member = declaration != null ? ReadListMember(declaration, "ListElementLabelName") as string : null;
                    object value = string.IsNullOrEmpty(member) ? null : ReadListMember(ResolveListMemberObject(property, path), member);
                    if (value != null)
                        attributes.Add(new SerializedPropertyAttributeInfo { type = "Locus.ListElementLabel", displayName = "ListElementLabel", value = Convert.ToString(value, CultureInfo.InvariantCulture) });
                }
                else
                {
                    int dot = path.LastIndexOf('.');
                    object owner = ResolveListMemberObject(property, dot < 0 ? "" : path.Substring(0, dot));
                    bool visible = true;
                    foreach (object declaration in declarations)
                    {
                        string type = declaration.GetType().FullName;
                        if (type != "Sirenix.OdinInspector.ShowIfAttribute" && type != "Sirenix.OdinInspector.HideIfAttribute") continue;
                        string condition = ReadListMember(declaration, "Condition") as string;
                        object actual = ReadListMember(owner, condition);
                        object expected = ReadListMember(declaration, "Value");
                        // Unknown members/expressions remain visible rather than hiding editable data.
                        if (actual == null) continue;
                        bool matches = expected != null ? Equals(actual, expected) : actual is bool && (bool)actual;
                        visible &= type.EndsWith(".ShowIfAttribute", StringComparison.Ordinal) ? matches : !matches;
                    }
                    if (!visible) attributes.Add(new SerializedPropertyAttributeInfo { type = "Locus.PropertyVisibility", displayName = "PropertyVisibility", value = "false" });
                }
                snapshot.attributes = attributes.ToArray();
            }
            catch (Exception)
            {
                // Author getters may throw while assets are incomplete; raw property
                // editing must remain available even when presentation cannot resolve.
            }
        }

        private static bool PropertyRestoreContainsManagedReference(PropertyRestoreNode node)
        {
            return node.type == "ManagedReference" || (node.children ?? new PropertyRestoreNode[0]).Any(PropertyRestoreContainsManagedReference);
        }

        private static void DuplicateSerializedListElement(SerializedProperty property, int index)
        {
            if (index < 0 || index >= property.arraySize) throw new ArgumentOutOfRangeException("index");
            var source = CapturePropertyRestoreState(property.GetArrayElementAtIndex(index));
            if (PropertyRestoreContainsManagedReference(source))
                throw new InvalidOperationException("Managed reference elements require an explicit graph copy.");
            property.InsertArrayElementAtIndex(index + 1);
            RestorePropertyState(property.GetArrayElementAtIndex(index + 1), source, new Dictionary<long, object>(), new HashSet<long>());
        }

        private static void InsertInitializedListElement(SerializedProperty property, int index)
        {
            Type elementType = SerializedElementType(ResolveSerializedPropertyFieldType(property));
            if (elementType == null) throw new InvalidOperationException("List element type is unavailable.");
            object initial = elementType == typeof(string) ? ""
                : typeof(UnityEngine.Object).IsAssignableFrom(elementType) || elementType.IsAbstract || elementType.IsInterface ? null
                : elementType.IsArray ? Array.CreateInstance(elementType.GetElementType(), 0)
                : Activator.CreateInstance(elementType, true);
            property.InsertArrayElementAtIndex(index);
            var element = property.GetArrayElementAtIndex(index);
            if (element.propertyType == SerializedPropertyType.ManagedReference) element.managedReferenceValue = null;
            else if (element.isArray && element.propertyType == SerializedPropertyType.Generic) element.arraySize = 0;
#if UNITY_2022_1_OR_NEWER
            else element.boxedValue = initial;
#else
            else
            {
                // Batched writes apply the shared SerializedObject; never leave a
                // half-initialized element behind when a field type is unsupported.
                try { WriteInitialSerializedValue(element, initial); }
                catch { property.DeleteArrayElementAtIndex(index); throw; }
            }
#endif
        }

#if !UNITY_2022_1_OR_NEWER
        // SerializedProperty.boxedValue is Unity 2022.1+. Write the fresh element
        // field by field so it does not keep the neighbour that
        // InsertArrayElementAtIndex copied. A null value writes the type default.
        private static void WriteInitialSerializedValue(SerializedProperty property, object value)
        {
            if (property.isFixedBuffer)
            {
                var items = value as IList;
                for (int i = 0; i < property.fixedBufferSize; i++)
                    WriteInitialSerializedValue(property.GetFixedBufferElementAtIndex(i), items != null && i < items.Count ? items[i] : null);
                return;
            }
            switch (property.propertyType)
            {
                case SerializedPropertyType.ManagedReference: property.managedReferenceValue = value; return;
                case SerializedPropertyType.ObjectReference: property.objectReferenceValue = value as UnityEngine.Object; return;
                case SerializedPropertyType.ExposedReference: property.exposedReferenceValue = value as UnityEngine.Object; return;
                case SerializedPropertyType.Boolean: property.boolValue = value is bool flag && flag; return;
                case SerializedPropertyType.Float: property.doubleValue = value == null ? 0 : Convert.ToDouble(value, CultureInfo.InvariantCulture); return;
                case SerializedPropertyType.String: property.stringValue = value as string ?? ""; return;
                case SerializedPropertyType.Character: property.intValue = value is char character ? character : 0; return;
                case SerializedPropertyType.LayerMask: property.intValue = value is UnityEngine.LayerMask mask ? mask.value : 0; return;
                case SerializedPropertyType.Integer:
                case SerializedPropertyType.Enum:
                    if (value == null) property.longValue = 0;
                    else if (value is ulong || (value is Enum && Enum.GetUnderlyingType(value.GetType()) == typeof(ulong)))
                        SetSerializedUInt64(property, Convert.ToUInt64(value, CultureInfo.InvariantCulture));
                    else property.longValue = Convert.ToInt64(value, CultureInfo.InvariantCulture);
                    return;
                case SerializedPropertyType.Color:
                    property.colorValue = value is UnityEngine.Color32 color32 ? (UnityEngine.Color)color32
                        : value is UnityEngine.Color color ? color : default(UnityEngine.Color);
                    return;
                case SerializedPropertyType.Vector2: property.vector2Value = value is UnityEngine.Vector2 vector2 ? vector2 : default(UnityEngine.Vector2); return;
                case SerializedPropertyType.Vector3: property.vector3Value = value is UnityEngine.Vector3 vector3 ? vector3 : default(UnityEngine.Vector3); return;
                case SerializedPropertyType.Vector4: property.vector4Value = value is UnityEngine.Vector4 vector4 ? vector4 : default(UnityEngine.Vector4); return;
                case SerializedPropertyType.Quaternion: property.quaternionValue = value is UnityEngine.Quaternion rotation ? rotation : default(UnityEngine.Quaternion); return;
                case SerializedPropertyType.Rect: property.rectValue = value is UnityEngine.Rect rect ? rect : default(UnityEngine.Rect); return;
                case SerializedPropertyType.Bounds: property.boundsValue = value is UnityEngine.Bounds bounds ? bounds : default(UnityEngine.Bounds); return;
                case SerializedPropertyType.Vector2Int: property.vector2IntValue = value is UnityEngine.Vector2Int vector2Int ? vector2Int : default(UnityEngine.Vector2Int); return;
                case SerializedPropertyType.Vector3Int: property.vector3IntValue = value is UnityEngine.Vector3Int vector3Int ? vector3Int : default(UnityEngine.Vector3Int); return;
                case SerializedPropertyType.RectInt: property.rectIntValue = value is UnityEngine.RectInt rectInt ? rectInt : default(UnityEngine.RectInt); return;
                case SerializedPropertyType.BoundsInt: property.boundsIntValue = value is UnityEngine.BoundsInt boundsInt ? boundsInt : default(UnityEngine.BoundsInt); return;
                case SerializedPropertyType.Hash128: property.hash128Value = value is UnityEngine.Hash128 hash ? hash : default(UnityEngine.Hash128); return;
                case SerializedPropertyType.AnimationCurve: property.animationCurveValue = value as UnityEngine.AnimationCurve ?? new UnityEngine.AnimationCurve(); return;
                case SerializedPropertyType.Generic:
                    if (property.isArray)
                    {
                        var items = value as IList;
                        property.arraySize = items != null ? items.Count : 0;
                        for (int i = 0; i < property.arraySize; i++)
                            WriteInitialSerializedValue(property.GetArrayElementAtIndex(i), items[i]);
                        return;
                    }
                    SerializedProperty child = property.Copy(), end = child.GetEndProperty();
                    bool enter = true;
                    while (child.Next(enter) && !SerializedProperty.EqualContents(child, end))
                    {
                        enter = false;
                        FieldInfo field = value == null ? null : SerializedMemberField(value.GetType(), child.name);
                        WriteInitialSerializedValue(child.Copy(), field != null ? field.GetValue(value) : null);
                    }
                    return;
                default:
                    // Gradient has no public setter before 2022.1.
                    throw new NotSupportedException("Initializing " + property.propertyType + " list elements requires Unity 2022.1 or newer.");
            }
        }
#endif

        private static void InsertListReferences(SerializedProperty property, int index, string[] paths)
        {
            Type elementType = SerializedElementType(ResolveSerializedPropertyFieldType(property));
            if (elementType == null || !typeof(UnityEngine.Object).IsAssignableFrom(elementType))
                throw new InvalidOperationException("This list does not accept object references.");
            if (paths == null || paths.Length == 0 || paths.Length > 1024) throw new ArgumentException("Invalid reference count.");
            // Resolve and validate the whole batch before changing the collection.
            var objects = paths.Select(path => {
                var value = LoadAssetAtPathForSerializedReference(path, elementType) ?? ResolveSubAssetReference(path, elementType);
                if (value == null || !IsSerializedObjectReferenceCompatible(value, elementType))
                    throw new InvalidOperationException("Object reference type does not match: " + path);
                return value;
            }).ToArray();
            for (int i = 0; i < objects.Length; i++) {
                property.InsertArrayElementAtIndex(index + i);
                property.GetArrayElementAtIndex(index + i).objectReferenceValue = objects[i];
            }
        }
    }
}
