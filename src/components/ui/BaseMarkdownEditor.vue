<script setup lang="ts">
import { Compartment, EditorSelection, EditorState, StateEffect, Transaction, type Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { openSearchPanel } from "@codemirror/search";
import { useSearchScope } from "../../composables/useSearchScope";
import { markdownEditorSearch } from "./markdown-editor/markdownEditorSearch";
import { computed, getCurrentInstance, inject, nextTick, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { WORKBENCH_FILE_OPEN_KEY, openWorkbenchFileTab } from "../../services/workbenchFile";
import { openMarkdownReference, relativeMarkdownPath } from "../../services/markdownReference";
import { useOpenView } from "../../composables/useOpenView";
import { extractKnowledgeDocumentOutline } from "../knowledge/knowledgeDocumentOutline";
import { openUrl } from "@tauri-apps/plugin-opener";
import { normalizeExternalMarkdownHref } from "../../composables/markdownExternalLinks";
import "../../styles/markdown-links.css";
import BaseButton from "./BaseButton.vue";
import { useDocumentRecovery } from "../../composables/useDocumentRecovery";
import { markdownImagePaste } from "./markdown-editor/markdownImagePaste";
import { markdownMentions } from "./markdown-editor/markdownMentions";
import { getLocusRuntime } from "../../services/locusRuntime";
import { rebaseDocumentText } from "../../document/documentText";
import MarkdownEditorControls from "./markdown-editor/MarkdownEditorControls.vue";
import MarkdownProperties from "./MarkdownProperties.vue";
import { markdownFrontmatterField, markdownFrontmatterHiding, type MarkdownFrontmatter } from "./markdown-editor/markdownFrontmatter";
import { isolateHistory } from "@codemirror/commands";
import MarkdownEditorContextMenu from "./markdown-editor/MarkdownEditorContextMenu.vue";
import {
  captureMarkdownEditorSelection,
  syncMarkdownDOMSelection,
  type MarkdownEditorSelection,
} from "./markdown-editor/markdownEditorSelection";
import { useNotificationStore } from "../../stores/notification";
import { t } from "../../i18n";
import {
  markdownEditorBaseExtensions,
  markdownEditorLanguageExtension,
  markdownEditorLanguageFromPath,
  markdownEditorModeExtension,
  markdownEditorPlaceholderExtension,
  markdownEditorReadOnlyExtension,
} from "./markdown-editor/codeMirrorMarkdownExtensions";
import {
  MarkdownEditorSessionCache,
  type MarkdownEditorSessionStore,
} from "./markdown-editor/markdownEditorSessionCache";
import { createMinimalTextChange } from "./markdown-editor/markdownEditorTransactions";
import type { MarkdownEditorDocumentChange } from "./markdown-editor/markdownEditorDocumentChange";
import type { MarkdownImageResolver, MarkdownLivePreviewOptions } from "./markdown-editor/markdownComplexWidgets";
import type { MarkdownReferenceToken } from "./markdown-editor/markdownComplexTokens";
import { normalizeMarkdownEditorLineEndings } from "./markdownEditorFormatting";
import type { MarkdownEditorViewMode } from "./markdownEditorViewMode";
import { resolveMarkdownImage, rewriteMarkdownImageLinks } from "../../services/markdownImage";
import type { WorkspaceRef } from "../../services/project";
import { subscribeWorkspaceFileChanges } from "../../services/workspaceExplorer";
import type { RuntimeUnsubscribe } from "../../services/locusRuntime";
import { useTextViewerZoom } from "../../composables/useTextViewerZoom";
import { workspaceFileChangeMatches } from "../../composables/useFileChangeRevalidation";
import { useDocumentEditingSettings } from "../../composables/useDocumentEditingSettings";
import { markdownPunctuationPairing } from "./markdown-editor/markdownPunctuationPairing";

const workspaceMarkdownImageResolver: MarkdownImageResolver = (source, context) => {
  if (!context.workspaceRef) return null;
  return resolveMarkdownImage(context.workspaceRef, source, context.contentPath);
};

const props = withDefaults(defineProps<{
  modelValue: string;
  savedValue?: string;
  disabled?: boolean;
  placeholder?: string;
  viewMode?: MarkdownEditorViewMode;
  contentPath?: string;
  /** Physical document location when contentPath names a logical knowledge field. */
  resourcePath?: string;
  contentKey?: string;
  sessionCache?: MarkdownEditorSessionStore | null;
  sessionPinned?: boolean;
  workspaceRef?: WorkspaceRef | null;
  active?: boolean;
  autoGrow?: boolean;
  minHeight?: number;
  canQuoteSelection?: boolean;
  showProperties?: boolean;
  searchPriority?: number;
  /**
   * Emits immutable CodeMirror Text/ChangeSet payloads for local edits. This
   * avoids allocating the whole document string on every transaction. The
   * model is materialized at an explicit shortcut-save boundary.
   */
  transactionModel?: boolean;
}>(), {
  disabled: false,
  placeholder: "",
  viewMode: "rendered",
  contentPath: "",
  contentKey: "",
  sessionCache: null,
  sessionPinned: false,
  workspaceRef: null,
  active: true,
  autoGrow: false,
  minHeight: 80,
  canQuoteSelection: false,
  showProperties: false,
  transactionModel: false,
});

const emit = defineEmits<{
  (e: "quoteSelection", selection: MarkdownEditorSelection): void;
  /** Settled user selection; null once it collapses or the editor shows another document. */
  (e: "selectionChange", selection: MarkdownEditorSelection | null): void;
  (e: "update:modelValue", value: string): void;
  (e: "documentChange", value: MarkdownEditorDocumentChange): void;
  (e: "shortcutSave"): void;
  (e: "externalChange"): void;
  (e: "referenceOpen", reference: MarkdownReferenceToken): void;
  (e: "referencePointerDown", payload: {
    reference: MarkdownReferenceToken;
    event: PointerEvent;
    element: HTMLElement;
  }): void;
}>();

const mountRef = ref<HTMLDivElement | null>(null);
const instance = getCurrentInstance();
const openWorkbenchFile = inject(WORKBENCH_FILE_OPEN_KEY, openWorkbenchFileTab);
const openViewPackage = useOpenView();
const searchRoot = ref<HTMLElement | null>(null);
useSearchScope(searchRoot, () => { if (editorView) openSearchPanel(editorView); }, {
  enabled: () => props.active && !!editorView,
  priority: props.searchPriority ?? 1,
});
const controlsRef = ref<InstanceType<typeof MarkdownEditorControls> | null>(null);
const contextMenuRef = ref<InstanceType<typeof MarkdownEditorContextMenu> | null>(null);
const controlsView = shallowRef<EditorView | null>(null);
const frontmatter = shallowRef<MarkdownFrontmatter | null>(null);
const propertiesCompartment = new Compartment();

function propertiesExtensions(): Extension {
  if (!props.showProperties || currentLanguage() !== "markdown") return [];
  return [markdownFrontmatterField, ...(props.viewMode === "rendered" ? markdownFrontmatterHiding : [])];
}

function updateProperties(yaml: string): void {
  const view = editorView;
  if (!view || view.state.readOnly) return;
  const current = view.state.field(markdownFrontmatterField, false);
  if (!current || current.invalid) return;
  const range = current.range;
  const change = range
    ? { from: range.yamlFrom, to: range.yamlTo, insert: yaml }
    : { from: view.state.doc.sliceString(0, 1) === "\uFEFF" ? 1 : 0, insert: `---\n${yaml}---\n\n` };
  view.dispatch({ changes: change, annotations: isolateHistory.of("full"), userEvent: "input.property" });
}
const languageCompartment = new Compartment();
const modeCompartment = new Compartment();
const readOnlyCompartment = new Compartment();
const placeholderCompartment = new Compartment();
const punctuationCompartment = new Compartment();
const { state: documentEditingSettings } = useDocumentEditingSettings();
const localSessionCache = new MarkdownEditorSessionCache();
const imageCacheEpoch = ref(0);
const {
  textViewerZoomScale,
  textViewerZoomStyle,
  handleTextViewerZoomWheel,
} = useTextViewerZoom();

let editorView: EditorView | null = null;
let activeSessionKey = normalizeSessionKey(props.contentKey);
let activeSessionCache = props.sessionCache ?? localSessionCache;
let activeSessionPinned = props.sessionPinned;
let applyingExternalModel = false;
let composing = false;
let pendingExternalModel: string | null = null;
let compositionFlushToken = 0;
let currentScrollTop = 0;
let currentScrollLeft = 0;
let removeScrollTracking: (() => void) | null = null;
let activeScrollElement: HTMLElement | null = null;
let releaseWorkspaceFileChanges: RuntimeUnsubscribe | null = null;
let editorUnmounted = false;
const physicalPath = computed(() => props.resourcePath || props.contentPath);
const recovery = useDocumentRecovery({
  key: () => JSON.stringify([props.workspaceRef?.checkoutId ?? "local", props.workspaceRef?.expectedMaterializationEpoch ?? 0, physicalPath.value, props.contentKey.match(/:(body|summary|maintenanceRules)$/)?.[1] ?? "body"]),
  base: () => normalizeMarkdown(props.savedValue ?? props.modelValue),
  text: () => editorView?.state.doc ?? props.modelValue,
  enabled: () => !props.disabled && !!physicalPath.value && currentLanguage() === "markdown",
  apply: (text) => {
    const view = editorView;
    if (!view || view.state.readOnly) return;
    const changes = createMinimalTextChange(view.state.doc.toString(), text);
    if (changes) view.dispatch({ changes, annotations: isolateHistory.of("full"), userEvent: "input.restore" });
    view.focus();
  },
});
let releaseImageRenames: RuntimeUnsubscribe | null = null;
const SELECTION_CHANGE_DELAY_MS = 150;
let selectionChangeTimer: ReturnType<typeof setTimeout> | null = null;
// Identifies the last emitted selection ("" = none), so cursor moves and
// repeated DOM syncs do not re-emit an unchanged selection.
let emittedSelectionKey = "";
let selectionDocument: Document | null = null;

function hasSelectionListener(): boolean {
  return !!instance?.vnode.props?.onSelectionChange;
}

function selectionKey(state: EditorState): string {
  const ranges = state.selection.ranges.filter((range) => !range.empty);
  return ranges.length
    ? `${state.doc.length}|${ranges.map((range) => `${range.from}:${range.to}`).join(",")}`
    : "";
}

function emitSelectionChange(): void {
  if (selectionChangeTimer) clearTimeout(selectionChangeTimer);
  selectionChangeTimer = null;
  const view = editorView;
  if (!view) return;
  const key = selectionKey(view.state);
  if (key === emittedSelectionKey) return;
  emittedSelectionKey = key;
  emit("selectionChange", key ? captureMarkdownEditorSelection(view.state) : null);
}

function scheduleSelectionChange(): void {
  if (!hasSelectionListener()) return;
  if (selectionChangeTimer) clearTimeout(selectionChangeTimer);
  selectionChangeTimer = setTimeout(emitSelectionChange, SELECTION_CHANGE_DELAY_MS);
}

/** Another document replaces this one; its selection no longer applies. */
function resetSelectionChange(): void {
  if (selectionChangeTimer) clearTimeout(selectionChangeTimer);
  selectionChangeTimer = null;
  if (!emittedSelectionKey) return;
  emittedSelectionKey = "";
  emit("selectionChange", null);
}

/** Non-editable content keeps the browser selection; mirror it into the editor state. */
function handleDocumentSelectionChange(): void {
  const view = editorView;
  if (!view || !view.state.readOnly) return;
  const selection = view.dom.ownerDocument.getSelection();
  if (!selection?.anchorNode || !view.contentDOM.contains(selection.anchorNode)) return;
  if (!selection.isCollapsed) {
    syncMarkdownDOMSelection(view);
    return;
  }
  if (view.state.selection.ranges.some((range) => !range.empty)) {
    view.dispatch({ selection: EditorSelection.cursor(view.state.selection.main.head) });
  }
}

function normalizeMarkdown(value: string): string {
  return normalizeMarkdownEditorLineEndings(value);
}

function normalizeSessionKey(value: string): string {
  return value.trim() || "__default__";
}

function currentLanguage() {
  return markdownEditorLanguageFromPath(props.contentPath);
}

function handleReferenceOpen(reference: MarkdownReferenceToken): void {
  if (instance?.vnode.props?.onReferenceOpen) emit("referenceOpen", reference);
  else if (props.workspaceRef) void openMarkdownReference(reference, props.workspaceRef, openWorkbenchFile, undefined, openViewPackage).catch(handleContextMenuError);
}

function openLink(url: string): void {
  const external = normalizeExternalMarkdownHref(url);
  if (external) { void openUrl(external).catch(handleContextMenuError); return; }
  const decode = (value: string) => { try { return decodeURIComponent(value); } catch { return value; } };
  if (url.startsWith("#") && editorView) {
    const heading = decode(url.slice(1));
    const item = extractKnowledgeDocumentOutline(editorView.state.doc.toString()).find(item => item.text === heading || item.text.toLowerCase().replace(/\s+/g, "-") === heading.toLowerCase());
    if (item) editorView.dispatch({ effects: EditorView.scrollIntoView(item.from, { y: "start" }) });
    return;
  }
  if (!props.workspaceRef || (/^[a-z][a-z0-9+.-]*:/i.test(url) && !/^[a-z]:[\\/]/i.test(url))) return;
  const filePath = relativeMarkdownPath(physicalPath.value, decode(url.split("#", 1)[0] ?? url));
  void openWorkbenchFile({ workspaceRef: props.workspaceRef, filePath }).catch(handleContextMenuError);
}

function handleReferencePointerDown(
  reference: MarkdownReferenceToken,
  event: PointerEvent,
  element: HTMLElement,
): void {
  emit("referencePointerDown", { reference, event, element });
}

function currentLivePreviewOptions(): MarkdownLivePreviewOptions {
  const workspaceRef = props.workspaceRef;
  const options: MarkdownLivePreviewOptions = {
    onEditTarget: (target) => controlsRef.value?.open(target),
    onReferenceOpen: handleReferenceOpen,
    onReferencePointerDown: handleReferencePointerDown,
    onLinkOpen: openLink,
  };
  if (!workspaceRef) return options;
  return {
    ...options,
    imageResolver: workspaceMarkdownImageResolver,
    imageContext: {
      cacheKey: `${workspaceRef.checkoutId}:${workspaceRef.expectedGeneration ?? "current"}:${imageCacheEpoch.value}`,
      contentPath: physicalPath.value,
      workspaceRef: {
        checkoutId: workspaceRef.checkoutId,
        expectedGeneration: workspaceRef.expectedGeneration,
        expectedMaterializationEpoch: workspaceRef.expectedMaterializationEpoch,
      },
    },
  };
}

function emitCurrentValue(): void {
  if (!editorView) return;
  emit("update:modelValue", normalizeMarkdown(editorView.state.doc.toString()));
}

function emitShortcutSave(): void {
  emitCurrentValue();
  emit("shortcutSave");
}

function flushPendingExternalModel(): void {
  const pending = pendingExternalModel;
  pendingExternalModel = null;
  if (pending === null) return;
  syncExternalModel(pending);
}

function resetCompositionState(flushPending: boolean): void {
  compositionFlushToken += 1;
  composing = false;
  if (flushPending) flushPendingExternalModel();
  else pendingExternalModel = null;
}

function handleCompositionStart(): boolean {
  compositionFlushToken += 1;
  composing = true;
  return false;
}

function handleCompositionEnd(): boolean {
  composing = false;
  const flushToken = ++compositionFlushToken;
  void Promise.resolve().then(() => {
    if (composing || flushToken !== compositionFlushToken) return;
    flushPendingExternalModel();
  });
  return false;
}

function editorExtensions(): Extension[] {
  const language = currentLanguage();
  return [
    ...markdownEditorBaseExtensions(emitShortcutSave),
    markdownImagePaste({
      context: () => props.workspaceRef && physicalPath.value && currentLanguage() === "markdown"
        ? { workspaceRef: { ...props.workspaceRef }, contentPath: physicalPath.value, key: activeSessionKey } : null,
      onError: handleContextMenuError,
    }),
    markdownEditorSearch(),
    markdownMentions({
      context: () => props.active && props.workspaceRef && currentLanguage() === "markdown"
        ? { workspaceRef: props.workspaceRef, contentPath: physicalPath.value, key: activeSessionKey } : null,
    }),
    propertiesCompartment.of(propertiesExtensions()),
    languageCompartment.of(markdownEditorLanguageExtension(language)),
    modeCompartment.of(markdownEditorModeExtension(
      props.viewMode,
      language,
      currentLivePreviewOptions(),
    )),
    readOnlyCompartment.of(markdownEditorReadOnlyExtension(props.disabled)),
    placeholderCompartment.of(markdownEditorPlaceholderExtension(props.placeholder)),
    punctuationCompartment.of(punctuationExtensions()),
    EditorView.domEventHandlers({
      compositionstart: handleCompositionStart,
      compositionend: handleCompositionEnd,
    }),
    EditorView.updateListener.of((update) => {
      frontmatter.value = update.state.field(markdownFrontmatterField, false) ?? null;
      if (update.docChanged || update.selectionSet) {
        contextMenuRef.value?.close(false);
        scheduleSelectionChange();
      }
      controlsRef.value?.update(update);
      if (!update.docChanged) return;
      recovery.changed();
      if (applyingExternalModel) return;
      if (props.transactionModel) {
        emit("documentChange", {
          doc: update.state.doc,
          changes: update.changes,
        });
        return;
      }
      emit("update:modelValue", normalizeMarkdown(update.state.doc.toString()));
    }),
  ];
}

function createEditorState(value: string): EditorState {
  return EditorState.create({
    doc: normalizeMarkdown(value),
    extensions: editorExtensions(),
  });
}

function configurationEffects() {
  const language = currentLanguage();
  return [
    propertiesCompartment.reconfigure(propertiesExtensions()),
    languageCompartment.reconfigure(markdownEditorLanguageExtension(language)),
    modeCompartment.reconfigure(markdownEditorModeExtension(
      props.viewMode,
      language,
      currentLivePreviewOptions(),
    )),
    readOnlyCompartment.reconfigure(markdownEditorReadOnlyExtension(props.disabled)),
    placeholderCompartment.reconfigure(markdownEditorPlaceholderExtension(props.placeholder)),
    punctuationCompartment.reconfigure(punctuationExtensions()),
  ];
}

function punctuationExtensions(): Extension {
  return documentEditingSettings.punctuationPairing && currentLanguage() === "markdown"
    ? markdownPunctuationPairing() : [];
}

function stateWithFreshConfiguration(state: EditorState): EditorState {
  return state.update({
    effects: StateEffect.reconfigure.of(editorExtensions()),
    annotations: Transaction.addToHistory.of(false),
  }).state;
}

function stateWithExternalValue(state: EditorState, value: string): EditorState {
  const normalized = normalizeMarkdown(value);
  const change = createMinimalTextChange(state.doc.toString(), normalized);
  if (!change) return state;
  return state.update({
    changes: change,
    annotations: Transaction.addToHistory.of(false),
  }).state;
}

function syncExternalModel(value: string): void {
  const view = editorView;
  if (!view) return;
  const normalized = normalizeMarkdown(value);
  const change = createMinimalTextChange(view.state.doc.toString(), normalized);
  if (!change) return;

  applyingExternalModel = true;
  try {
    view.dispatch({
      changes: change,
      annotations: Transaction.addToHistory.of(false),
    });
  } finally {
    applyingExternalModel = false;
  }
}

function syncOrQueueExternalModel(value: string): void {
  const normalized = normalizeMarkdown(value);
  if (composing && editorView) {
    pendingExternalModel = normalized;
    return;
  }
  compositionFlushToken += 1;
  pendingExternalModel = null;
  syncExternalModel(normalized);
}

function saveActiveSession(
  modelValue = props.modelValue,
  pinned = activeSessionPinned,
): void {
  const view = editorView;
  if (!view) return;
  activeSessionCache.set(activeSessionKey, {
    state: view.state,
    scrollTop: currentScrollTop,
    scrollLeft: currentScrollLeft,
    modelValue: normalizeMarkdown(modelValue),
    pinned,
  });
}

function isScrollableOverflow(element: HTMLElement): boolean {
  const style = element.ownerDocument.defaultView?.getComputedStyle(element);
  if (!style) return false;
  return /^(?:auto|scroll|overlay)$/.test(style.overflowY)
    || /^(?:auto|scroll|overlay)$/.test(style.overflow);
}

function resolveScrollElement(view: EditorView): HTMLElement {
  if (props.autoGrow) {
    let ancestor = view.dom.parentElement;
    while (ancestor) {
      if (isScrollableOverflow(ancestor)) return ancestor;
      ancestor = ancestor.parentElement;
    }
    const documentScroller = view.dom.ownerDocument.scrollingElement;
    if (documentScroller instanceof HTMLElement) return documentScroller;
  }
  return view.scrollDOM;
}

function startScrollTracking(view: EditorView): void {
  removeScrollTracking?.();
  const scrollElement = resolveScrollElement(view);
  activeScrollElement = scrollElement;
  const handleScroll = () => {
    currentScrollTop = scrollElement.scrollTop;
    currentScrollLeft = scrollElement.scrollLeft;
  };
  scrollElement.addEventListener("scroll", handleScroll, { passive: true });
  removeScrollTracking = () => {
    scrollElement.removeEventListener("scroll", handleScroll);
    if (activeScrollElement === scrollElement) activeScrollElement = null;
    removeScrollTracking = null;
  };
}

function restoreTrackedScroll(view: EditorView, top: number, left: number): void {
  const scrollElement = activeScrollElement ?? resolveScrollElement(view);
  scrollElement.scrollTop = top;
  scrollElement.scrollLeft = left;
}

function stateWithSessionModel(
  state: EditorState,
  cachedModelValue: string | undefined,
  nextModelValue: string,
): EditorState {
  if (
    props.transactionModel
    && cachedModelValue !== undefined
    && normalizeMarkdown(cachedModelValue) === normalizeMarkdown(nextModelValue)
  ) {
    return state;
  }
  return stateWithExternalValue(state, nextModelValue);
}

function restoreSession(contentKey: string, nextModelValue: string, previousModelValue: string): void {
  contextMenuRef.value?.close(false);
  const view = editorView;
  if (!view) {
    activeSessionKey = normalizeSessionKey(contentKey);
    return;
  }

  resetCompositionState(true);
  resetSelectionChange();
  saveActiveSession(previousModelValue, activeSessionPinned);
  activeSessionKey = normalizeSessionKey(contentKey);
  activeSessionCache = props.sessionCache ?? localSessionCache;
  activeSessionPinned = props.sessionPinned;
  activeSessionCache.setPinned(activeSessionKey, activeSessionPinned);
  const cached = activeSessionCache.get(activeSessionKey);
  // A newly created state already contains the current document and every
  // compartment. Re-applying both here duplicates the most expensive work for
  // large documents (full-text comparison plus language/plugin setup).
  const nextState = cached
    ? stateWithFreshConfiguration(stateWithSessionModel(cached.state, cached.modelValue, nextModelValue))
    : createEditorState(nextModelValue);
  view.setState(nextState);
  frontmatter.value = nextState.field(markdownFrontmatterField, false) ?? null;

  const scrollTop = cached?.scrollTop ?? 0;
  const scrollLeft = cached?.scrollLeft ?? 0;
  currentScrollTop = scrollTop;
  currentScrollLeft = scrollLeft;
  const restoredKey = activeSessionKey;
  view.requestMeasure({
    read: () => null,
    write() {
      if (!editorView || activeSessionKey !== restoredKey) return;
      restoreTrackedScroll(editorView, scrollTop, scrollLeft);
    },
  });
}

function mountEditor(): void {
  const parent = mountRef.value;
  if (!parent || editorView || !props.active) return;

  activeSessionCache = props.sessionCache ?? localSessionCache;
  activeSessionPinned = props.sessionPinned;
  activeSessionCache.setPinned(activeSessionKey, activeSessionPinned);
  const cached = activeSessionCache.get(activeSessionKey);
  const state = cached
    ? stateWithFreshConfiguration(stateWithSessionModel(cached.state, cached.modelValue, props.modelValue))
    : createEditorState(props.modelValue);
  editorView = new EditorView({ state, parent });
  frontmatter.value = state.field(markdownFrontmatterField, false) ?? null;
  controlsView.value = editorView;
  startScrollTracking(editorView);

  const scrollTop = cached?.scrollTop ?? 0;
  const scrollLeft = cached?.scrollLeft ?? 0;
  currentScrollTop = scrollTop;
  currentScrollLeft = scrollLeft;
  const restoredKey = activeSessionKey;
  editorView.requestMeasure({
    read: () => null,
    write() {
      if (!editorView || activeSessionKey !== restoredKey) return;
      restoreTrackedScroll(editorView, scrollTop, scrollLeft);
    },
  });
}

function suspendEditor(): void {
  if (!editorView) return;
  // A hidden tab keeps its last selection; deliver one that is still settling.
  if (selectionChangeTimer) emitSelectionChange();
  resetCompositionState(true);
  saveActiveSession();
  removeScrollTracking?.();
  editorView.destroy();
  editorView = null;
  controlsView.value = null;
}

function reconfigureEditor(): void {
  contextMenuRef.value?.close(false);
  const view = editorView;
  if (!view) return;
  applyingExternalModel = true;
  try {
    view.dispatch({
      effects: configurationEffects(),
      annotations: Transaction.addToHistory.of(false),
    });
  } finally {
    applyingExternalModel = false;
  }
  view.requestMeasure();
}

function handleEditorWheel(event: WheelEvent): void {
  handleTextViewerZoomWheel(event);
}

function handleContextMenuError(error: unknown): void {
  useNotificationStore().addNotice("error", `${t("editor.actionFailed")}: ${error instanceof Error ? error.message : String(error)}`);
}

onMounted(() => {
  mountEditor();
  if (hasSelectionListener()) {
    selectionDocument = mountRef.value?.ownerDocument ?? null;
    selectionDocument?.addEventListener("selectionchange", handleDocumentSelectionChange);
  }
  void getLocusRuntime().subscribe<{ checkoutId?: string; workspaceGeneration?: number; materializationEpoch?: number; oldPath: string; newPath: string }>("markdown-image-renamed", async (event) => {
    const workspaceRef = props.workspaceRef;
    const view = editorView;
    if (!workspaceRef || !view || view.state.readOnly || currentLanguage() !== "markdown" || (event.checkoutId && event.checkoutId !== workspaceRef.checkoutId)) return;
    if (event.workspaceGeneration != null && workspaceRef.expectedGeneration != null && event.workspaceGeneration !== workspaceRef.expectedGeneration) return;
    if (event.materializationEpoch != null && workspaceRef.expectedMaterializationEpoch != null && event.materializationEpoch !== workspaceRef.expectedMaterializationEpoch) return;
    const key = activeSessionKey;
    const before = view.state.doc.toString();
    try {
      const after = await rewriteMarkdownImageLinks(workspaceRef, physicalPath.value, before, event.oldPath, event.newPath);
      if (editorView !== view || activeSessionKey !== key || after === before) return;
      const rebased = rebaseDocumentText(before, view.state.doc.toString(), after);
      if (rebased.conflicts.length) return;
      const changes = createMinimalTextChange(view.state.doc.toString(), rebased.text);
      if (changes) view.dispatch({ changes, annotations: Transaction.addToHistory.of(false) });
      emit("externalChange");
    } catch (error) { handleContextMenuError(error); }
  }).then(release => { if (editorUnmounted) release(); else releaseImageRenames = release; }).catch(handleContextMenuError);
  void subscribeWorkspaceFileChanges((event) => {
    if (!workspaceFileChangeMatches(event, props.workspaceRef, event.payload.path)) return;
    if (!/\.(?:png|jpe?g|gif|bmp|webp|svg)$/i.test(event.payload.path)) return;
    const source = (editorView?.state.doc.toString() ?? props.modelValue).replace(/\\/g, "/");
    if (!source.toLocaleLowerCase().includes(event.payload.path.toLocaleLowerCase())) return;
    imageCacheEpoch.value += 1;
    reconfigureEditor();
  }).then((release) => {
    if (editorUnmounted) release();
    else releaseWorkspaceFileChanges = release;
  });
});

watch(
  () => props.active,
  (active) => {
    if (!active) {
      suspendEditor();
      return;
    }
    activeSessionKey = normalizeSessionKey(props.contentKey);
    activeSessionCache = props.sessionCache ?? localSessionCache;
    activeSessionPinned = props.sessionPinned;
    mountEditor();
  },
);

watch(
  () => [props.contentKey, props.modelValue] as const,
  ([nextKey, nextValue], [previousKey, previousValue]) => {
    if (nextKey !== previousKey) {
      restoreSession(nextKey, nextValue, previousValue);
      return;
    }
    if (nextValue !== previousValue) syncOrQueueExternalModel(nextValue);
  },
);

watch(
  () => [
    props.viewMode,
    documentEditingSettings.punctuationPairing,
    props.showProperties,
    props.disabled,
    props.placeholder,
    props.contentPath,
    props.resourcePath,
    props.workspaceRef?.checkoutId,
    props.workspaceRef?.expectedGeneration,
    props.workspaceRef?.expectedMaterializationEpoch,
  ] as const,
  () => reconfigureEditor(),
);

watch(
  () => props.sessionPinned,
  (pinned) => {
    activeSessionPinned = pinned;
    if (editorView) {
      saveActiveSession(props.modelValue, pinned);
      return;
    }
    activeSessionCache.setPinned(activeSessionKey, pinned);
  },
);

watch(
  () => [props.autoGrow, props.minHeight] as const,
  () => {
    void nextTick(() => {
      if (!editorView) return;
      startScrollTracking(editorView);
      editorView.requestMeasure();
    });
  },
);

watch(textViewerZoomScale, () => {
  void nextTick(() => editorView?.requestMeasure());
});

onBeforeUnmount(() => {
  editorUnmounted = true;
  selectionDocument?.removeEventListener("selectionchange", handleDocumentSelectionChange);
  selectionDocument = null;
  if (selectionChangeTimer) clearTimeout(selectionChangeTimer);
  selectionChangeTimer = null;
  releaseWorkspaceFileChanges?.();
  releaseWorkspaceFileChanges = null;
  releaseImageRenames?.();
  suspendEditor();
  localSessionCache.clear();
});

defineExpose({
  getEditorView: () => editorView,
  getBodyElement: () => mountRef.value,
});
</script>

<template>
  <div
    class="base-markdown-editor"
    ref="searchRoot"
    :class="{
      disabled,
      'auto-grow': autoGrow,
      'is-rendered': viewMode === 'rendered',
      'is-source': viewMode === 'native',
    }"
    :style="{
      '--markdown-editor-min-height': `${minHeight}px`,
      ...textViewerZoomStyle,
    }"
    @wheel="handleEditorWheel"
  >
    <div v-if="recovery.pending.value" class="markdown-recovery-bar" role="status">
      <span>有未保存的草稿</span>
      <BaseButton size="sm" @click="recovery.restore(recovery.pending.value!)">恢复草稿</BaseButton>
      <BaseButton size="sm" @click="recovery.dismiss">保留当前内容</BaseButton>
    </div>
    <div v-if="recovery.error.value" class="markdown-recovery-bar" role="alert">{{ recovery.error.value }}</div>
    <section v-if="recovery.historyOpen.value" class="markdown-recovery-history" aria-label="版本历史">
      <header><span>版本历史</span><BaseButton size="sm" @click="recovery.historyOpen.value = false">关闭</BaseButton></header>
      <p v-if="!recovery.snapshots.value.length">暂无历史版本</p>
      <details v-for="snapshot in [...recovery.snapshots.value].reverse()" :key="snapshot.at">
        <summary>{{ new Date(snapshot.at).toLocaleString() }}</summary>
        <pre>{{ snapshot.text }}</pre><BaseButton size="sm" :disabled="disabled" @click="recovery.restore(snapshot)">恢复此版本</BaseButton>
      </details>
    </section>
    <MarkdownProperties
      v-if="showProperties && frontmatter && viewMode === 'rendered' && (!disabled || frontmatter.range)"
      :key="contentKey"
      :yaml="frontmatter.yaml"
      :disabled="disabled"
      :error="frontmatter.invalid ? t('editor.properties.invalidYaml') : ''"
      @change="updateProperties"
      @shortcut-save="emitShortcutSave"
    />
    <div ref="mountRef" class="base-markdown-editor-host" @contextmenu="contextMenuRef?.open($event)" @keydown="contextMenuRef?.onKeydown($event)" />
    <MarkdownEditorContextMenu v-if="controlsView" ref="contextMenuRef" :view="controlsView" :can-history="!!physicalPath && currentLanguage() === 'markdown'" :can-quote="canQuoteSelection" @history="recovery.openHistory" @quote-selection="emit('quoteSelection', $event)" @action-error="handleContextMenuError" />
    <MarkdownEditorControls v-if="controlsView && viewMode === 'rendered' && currentLanguage() === 'markdown' && !disabled" :key="contentKey" ref="controlsRef" :view="controlsView" :workspace-ref="workspaceRef" :content-path="physicalPath" />
  </div>
</template>

<style scoped>
.markdown-recovery-bar { display: flex; align-items: center; gap: 8px; padding: 6px 12px; color: var(--text-secondary); font-size: 12px; }
.markdown-recovery-history { padding: 12px; border-bottom: 1px solid var(--border-color); font-size: 12px; }
.markdown-recovery-history header { display: flex; justify-content: space-between; align-items: center; }
.markdown-recovery-history details { margin-top: 8px; }
.markdown-recovery-history summary { cursor: pointer; }
.markdown-recovery-history pre { max-height: 240px; overflow: auto; white-space: pre-wrap; font-family: var(--font-mono-editor); }
.base-markdown-editor {
  --markdown-document-font-size: calc(14px * var(--text-viewer-font-scale, 1));
  --markdown-source-font-size: calc(13px * var(--text-viewer-font-scale, 1));
  --markdown-document-line-height: 1.68;
  --markdown-document-list-indent: 20px;
  --markdown-document-block-spacing: 8px;
  --markdown-document-padding-left: 16px;
  --markdown-document-padding-right: 14px;
  display: flex;
  flex: 1 1 0;
  flex-direction: column;
  width: 100%;
  height: 100%;
  min-width: 0;
  min-height: 0;
  background: transparent;
}

.base-markdown-editor-host {
  display: flex;
  flex: 1;
  width: 100%;
  min-width: 0;
  min-height: 0;
}

.base-markdown-editor :deep(.cm-editor) {
  flex: 1;
  width: 100%;
  min-width: 0;
  min-height: var(--markdown-editor-min-height);
  font-size: var(--markdown-document-font-size);
  background: transparent;
}

.base-markdown-editor :deep(.cm-scroller) {
  min-width: 0;
  min-height: 0;
  overflow: auto;
  overscroll-behavior: contain;
  font-family: var(--font-prose);
  font-size: inherit;
  line-height: var(--markdown-document-line-height);
}

.base-markdown-editor :deep(.cm-content) {
  min-height: 100%;
  padding: 14px 0 16px;
  color: var(--text-color);
  caret-color: var(--accent-color);
}

.base-markdown-editor :deep(.cm-line) {
  /* CodeMirror derives full-line selection bounds from .cm-line padding. */
  padding: 0 var(--markdown-document-padding-right) 0 var(--markdown-document-padding-left);
}

.base-markdown-editor :deep(.cm-line.cm-live-table-line) {
  padding: 0;
}

.base-markdown-editor :deep(.cm-live-preview .cm-widgetBuffer) {
  /* Keep the 1em caret target inside the line box. text-top alignment can
     enlarge it when hidden syntax is replaced beside mixed-font text. */
  vertical-align: -0.2em;
}

.base-markdown-editor.disabled,
.base-markdown-editor.disabled :deep(.cm-content) {
  cursor: default;
}

.base-markdown-editor :deep(.cm-editor.cm-source-mode) {
  font-size: var(--markdown-source-font-size);
}

.base-markdown-editor :deep(.cm-editor.cm-source-mode .cm-scroller) {
  font-family: var(--font-mono-editor);
  font-size: inherit;
  line-height: 1.65;
}

.base-markdown-editor.auto-grow {
  flex: none;
  height: auto;
  min-height: var(--markdown-editor-min-height);
}

.base-markdown-editor.auto-grow .base-markdown-editor-host,
.base-markdown-editor.auto-grow :deep(.cm-editor),
.base-markdown-editor.auto-grow :deep(.cm-scroller) {
  flex: none;
  height: auto;
  min-height: var(--markdown-editor-min-height);
  overflow: visible;
  overscroll-behavior: auto;
}

.base-markdown-editor.auto-grow :deep(.cm-content) {
  min-height: var(--markdown-editor-min-height);
}

.base-markdown-editor :deep(.cm-live-heading) {
  color: var(--text-color);
  font-weight: 600;
  line-height: 1.35;
  padding-top: 14px;
  padding-bottom: 6px;
}

.base-markdown-editor :deep(.cm-live-heading-1) {
  font-size: 1.4em;
  padding-bottom: 10px;
}

.base-markdown-editor :deep(.cm-live-heading-2) {
  font-size: 1.2em;
  padding-bottom: 8px;
  border-bottom: 1px solid color-mix(in srgb, var(--border-color) 84%, transparent);
}

.base-markdown-editor :deep(.cm-live-heading-3) {
  font-size: 1.08em;
}

.base-markdown-editor :deep(.cm-live-heading-4),
.base-markdown-editor :deep(.cm-live-heading-5),
.base-markdown-editor :deep(.cm-live-heading-6) {
  font-size: 1em;
  color: var(--text-secondary);
}

.base-markdown-editor :deep(.cm-live-heading-marker),
.base-markdown-editor :deep(.cm-live-syntax-marker) {
  color: color-mix(in srgb, var(--accent-color) 68%, var(--text-secondary) 32%);
  font-weight: 400;
  font-style: normal;
  /* Revealing syntax may consume width, but must not add font leading. */
  line-height: 0;
}

.base-markdown-editor :deep(.cm-live-heading-marker) {
  font-family: var(--font-mono-inline);
  font-size: 0.85em;
}

.base-markdown-editor :deep(.cm-live-heading-marker span),
.base-markdown-editor :deep(.cm-live-syntax-marker span) {
  color: inherit;
  font-weight: inherit;
  font-style: inherit;
}

.base-markdown-editor :deep(.cm-live-strong) {
  font-weight: 600;
}

.base-markdown-editor :deep(.cm-live-emphasis) {
  color: color-mix(in srgb, var(--text-color) 82%, var(--text-secondary) 18%);
  font-style: italic;
}

.base-markdown-editor :deep(.cm-live-strikethrough) {
  text-decoration: line-through;
  text-decoration-color: color-mix(in srgb, var(--text-secondary) 78%, transparent);
}

.base-markdown-editor :deep(.cm-live-inline-code) {
  padding: 1px 5px;
  border: 1px solid color-mix(in srgb, var(--border-color) 78%, transparent);
  border-radius: 4px;
  background: color-mix(in srgb, var(--sidebar-bg) 52%, transparent);
  color: color-mix(in srgb, var(--text-color) 92%, var(--accent-color) 8%);
  font-family: var(--font-mono-inline);
  font-size: 0.92em;
}

.base-markdown-editor :deep(.cm-live-list-marker) {
  display: inline-block;
  min-width: var(--markdown-document-list-indent);
  color: var(--text-secondary);
  font-weight: 600;
  text-align: center;
  user-select: none;
}

.base-markdown-editor :deep(.cm-live-task-checkbox) {
  width: 14px;
  height: 14px;
  margin: 0 6px 0 1px;
  vertical-align: -2px;
  accent-color: var(--accent-color);
}

.base-markdown-editor :deep(.cm-live-blockquote) {
  margin-right: var(--markdown-document-padding-right);
  margin-left: var(--markdown-document-padding-left);
  padding-right: 0;
  padding-left: 12px;
  border-left: 2px solid color-mix(in srgb, var(--accent-color) 38%, var(--border-color));
  background: color-mix(in srgb, var(--sidebar-bg) 44%, transparent);
  color: var(--text-secondary);
}

.base-markdown-editor :deep(.cm-live-blockquote-start) {
  /* CodeMirror measures line boxes without margins for pointer/caret mapping. */
  padding-top: var(--markdown-document-block-spacing);
}

.base-markdown-editor :deep(.cm-live-blockquote-end) {
  padding-bottom: var(--markdown-document-block-spacing);
}

.base-markdown-editor :deep(.cm-live-horizontal-rule) {
  display: inline-block;
  width: 100%;
  height: 0.8em;
  border-top: 1px solid var(--border-color);
  opacity: 0.8;
  vertical-align: middle;
}

.base-markdown-editor :deep(.cm-live-fenced-code) {
  margin-right: var(--markdown-document-padding-right);
  margin-left: var(--markdown-document-padding-left);
  padding: 0 12px;
  border-right: 1px solid color-mix(in srgb, var(--border-color) 86%, transparent);
  border-left: 1px solid color-mix(in srgb, var(--border-color) 86%, transparent);
  background: color-mix(in srgb, var(--sidebar-bg) 76%, transparent);
  font-family: var(--font-mono-block);
  font-size: 13px;
  line-height: 1.55;
}

.base-markdown-editor :deep(.cm-live-fenced-code-start) {
  padding-top: 9px;
  border-top: 1px solid color-mix(in srgb, var(--border-color) 86%, transparent);
  border-radius: 8px 8px 0 0;
}

.base-markdown-editor :deep(.cm-live-fenced-code-end) {
  padding-bottom: 9px;
  border-bottom: 1px solid color-mix(in srgb, var(--border-color) 86%, transparent);
  border-radius: 0 0 8px 8px;
}
</style>
