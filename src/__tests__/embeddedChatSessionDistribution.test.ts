// @vitest-environment jsdom
import { createApp, defineComponent, h, watch } from "vue";
import { createPinia, setActivePinia } from "pinia";
import { useNotificationStore } from "../stores/notification";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  publishSessionAsyncTaskUpdate,
  publishSessionExecutionState,
  publishSessionContentChange,
  publishSessionStreamEvent,
} from "../services/sessionStreamEventHub";
import { preloadEmbeddedChatSession, useEmbeddedChatSession } from "../composables/useEmbeddedChatSession";

const mocks = vi.hoisted(() => ({
  loadSession: vi.fn(),
  loadSessionView: vi.fn(),
  synchronizeSessionRemoteHistory: vi.fn(),
  loadSessionMessagePage: vi.fn(),
  loadSessionTurnPreview: vi.fn(),
  getSessionUsage: vi.fn(),
  getTodos: vi.fn(),
  getSessionResumeAvailable: vi.fn(),
  getSessionPlanState: vi.fn(),
  setSessionPlanMode: vi.fn(),
  forkSession: vi.fn(),
  forkSessionFromMessage: vi.fn(),
  rollbackSessionToMessage: vi.fn(),
  undoLatestConversationTurn: vi.fn(),
  undoList: vi.fn(),
  undoCheckConflicts: vi.fn(),
  undoCheckDirty: vi.fn(),
  undoPerform: vi.fn(),
  undoPerformToMessage: vi.fn(),
  chat: vi.fn(),
  queueSessionCompact: vi.fn(),
  cancelChat: vi.fn(),
  tauriListen: vi.fn().mockResolvedValue(vi.fn()),
  modelDefaults: {
    planModel: "",
    subagentModels: {},
    subagentEfforts: {},
    subagentFastModes: {},
  },
  availableModels: [{
    id: "model-a",
    name: "Model A",
    provider: "openrouter",
    contextWindow: 128_000,
    supportedEfforts: ["none", "low", "medium", "high"],
  }],
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: mocks.tauriListen,
}));

vi.mock("../services/session", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../services/session")>();
  return {
    ...actual,
    loadSession: mocks.loadSession,
    loadSessionView: mocks.loadSessionView,
    synchronizeSessionRemoteHistory: mocks.synchronizeSessionRemoteHistory,
    loadSessionMessagePage: mocks.loadSessionMessagePage,
    loadSessionTurnPreview: mocks.loadSessionTurnPreview,
    getSessionUsage: mocks.getSessionUsage,
    getTodos: mocks.getTodos,
    getSessionResumeAvailable: mocks.getSessionResumeAvailable,
    getSessionPlanState: mocks.getSessionPlanState,
    setSessionPlanMode: mocks.setSessionPlanMode,
    forkSession: mocks.forkSession,
    forkSessionFromMessage: mocks.forkSessionFromMessage,
    rollbackSessionToMessage: mocks.rollbackSessionToMessage,
    undoLatestConversationTurn: mocks.undoLatestConversationTurn,
    chat: mocks.chat,
    queueSessionCompact: mocks.queueSessionCompact,
    cancelChat: mocks.cancelChat,
  };
});

vi.mock("../stores/model", () => ({
  useModelStore: () => ({
    modelDefaults: mocks.modelDefaults,
    availableModels: mocks.availableModels,
  }),
}));

vi.mock("../services/undo", () => ({
  undoList: mocks.undoList,
  undoCheckConflicts: mocks.undoCheckConflicts,
  undoCheckDirty: mocks.undoCheckDirty,
  undoPerform: mocks.undoPerform,
  undoPerformToMessage: mocks.undoPerformToMessage,
}));

describe("embedded chat session distribution", () => {
  beforeEach(() => {
    setActivePinia(createPinia());
    mocks.loadSessionView.mockImplementation(async (sessionId: string) => {
      const session = await mocks.loadSession(sessionId);
      return {
        session,
        userMessageIds: session.messages
          .filter((message: { role: string }) => message.role === "user")
          .map((message: { id: string }) => message.id),
        oldestMessageRowId: null,
        hasMoreHistory: false,
      };
    });
    mocks.synchronizeSessionRemoteHistory.mockResolvedValue({ changed: false, warning: null });
    mocks.getSessionUsage.mockResolvedValue(null);
    mocks.getTodos.mockResolvedValue({ items: [], latestRunId: null });
    mocks.getSessionResumeAvailable.mockResolvedValue(false);
    mocks.getSessionPlanState.mockResolvedValue({
      active: false,
      planFilePath: "",
      planFileExists: false,
    });
    mocks.undoList.mockResolvedValue([]);
    mocks.undoCheckConflicts.mockResolvedValue([]);
    mocks.undoCheckDirty.mockResolvedValue([]);
    mocks.undoPerform.mockResolvedValue(undefined);
    mocks.undoPerformToMessage.mockResolvedValue(undefined);
    mocks.modelDefaults.planModel = "";
    mocks.availableModels.splice(0, mocks.availableModels.length, {
      id: "model-a",
      name: "Model A",
      provider: "openrouter",
      contextWindow: 128_000,
      supportedEfforts: ["none", "low", "medium", "high"],
    });
  });

  afterEach(() => {
    vi.useRealTimers();
    mocks.loadSession.mockReset();
    mocks.loadSessionView.mockReset();
    mocks.loadSessionMessagePage.mockReset();
    mocks.loadSessionTurnPreview.mockReset();
    mocks.getSessionUsage.mockReset();
    mocks.getTodos.mockReset();
    mocks.getSessionResumeAvailable.mockReset();
    mocks.getSessionPlanState.mockReset();
    mocks.setSessionPlanMode.mockReset();
    mocks.forkSession.mockReset();
    mocks.forkSessionFromMessage.mockReset();
    mocks.rollbackSessionToMessage.mockReset();
    mocks.undoLatestConversationTurn.mockReset();
    mocks.undoList.mockReset();
    mocks.undoCheckConflicts.mockReset();
    mocks.undoCheckDirty.mockReset();
    mocks.undoPerform.mockReset();
    mocks.undoPerformToMessage.mockReset();
    mocks.chat.mockReset();
    mocks.queueSessionCompact.mockReset();
    mocks.cancelChat.mockReset();
    mocks.tauriListen.mockClear();
  });

  it.each([true, false])("waits for archive restoration before sending and recovers a failed draft: %s", async (succeeds) => {
    const id = `archive-prelaunch-${succeeds}`;
    mocks.loadSession.mockImplementation(async () => ({
      id, title: id, sessionType: "chat", parentSessionId: null,
      createdAt: 1, updatedAt: 1, messages: [], pendingInputs: [],
    }));
    mocks.chat.mockResolvedValue({ sessionId: id, runId: `${id}-run` });
    let finish!: () => void;
    let fail!: (error: Error) => void;
    const beforeSessionLaunch = vi.fn(() => new Promise<void>((resolve, reject) => { finish = resolve; fail = reject; }));
    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const app = createApp(defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: `${id}-pane`, initialSessionId: id,
          workspaceRef: { checkoutId: "archive-checkout", expectedGeneration: 1 },
          selectedModelId: "model-a", beforeSessionLaunch,
          buildRequest: (text) => ({ text }),
        });
        return () => h("div");
      },
    }));
    app.mount(document.createElement("div"));
    try {
      await vi.waitFor(() => expect(session.isLoading.value).toBe(false));
      session.inputText.value = "Continue this archived conversation";
      const sending = session.send();
      expect(beforeSessionLaunch).toHaveBeenCalledWith(id);
      expect(mocks.chat).not.toHaveBeenCalled();
      if (succeeds) finish();
      else fail(new Error("Restore failed"));
      await sending;
      if (succeeds) {
        expect(mocks.chat).toHaveBeenCalledWith(expect.objectContaining({ sessionId: id, text: "Continue this archived conversation" }));
      } else {
        expect(mocks.chat).not.toHaveBeenCalled();
        expect(session.restoredComposerDraft.value?.text).toBe("Continue this archived conversation");
        expect(session.isStreaming.value).toBe(false);
      }
    } finally {
      app.unmount();
    }
  });

  it("restores and isolates Claude phases in workbench sessions without opening thinking", async () => {
    mocks.loadSession.mockImplementation(async (id: string) => ({
      id, title: id, sessionType: "chat", parentSessionId: null, createdAt: 1, updatedAt: 1, messages: [],
      runtime: id === "native-phase" ? {
        activeRun: { runId: "native-run", sessionId: id, status: "running", startedAt: 1, updatedAt: 1 },
        activeToolCalls: [], pendingToolConfirms: [], isCompacting: false, claudeCodePhase: "thinking",
      } : undefined,
    }));
    let native!: ReturnType<typeof useEmbeddedChatSession>;
    let other!: ReturnType<typeof useEmbeddedChatSession>;
    const app = createApp(defineComponent({
      setup() {
        const common = { workspaceRef: { checkoutId: "phase-checkout", expectedGeneration: 1 }, selectedModelId: "model-a", buildRequest: (text: string) => ({ text }) };
        native = useEmbeddedChatSession({ ...common, sessionKey: "native-phase-pane", initialSessionId: "native-phase" });
        other = useEmbeddedChatSession({ ...common, sessionKey: "other-phase-pane", initialSessionId: "other-phase" });
        return () => h("div");
      },
    }));
    app.mount(document.createElement("div"));
    try {
      await vi.waitFor(() => expect(native.claudeCodePhase.value).toBe("thinking"));
      expect(native.isThinking.value).toBe(false);
      const source = { kind: "workspace" as const, projectId: "project", checkoutId: "phase-checkout", workspaceGeneration: 1, streamRevision: 1 };
      publishSessionStreamEvent({ event: { type: "claudeCodePhase", sessionId: "native-phase", runId: "native-run", phase: "responding" }, source });
      expect(native.claudeCodePhase.value).toBe("responding");
      expect(other.claudeCodePhase.value).toBeNull();
      publishSessionStreamEvent({ event: { type: "done", sessionId: "native-phase", runId: "native-run", messageId: "answer", fullText: "" }, source: { ...source, streamRevision: 2 } });
      expect(native.claudeCodePhase.value).toBeNull();
      publishSessionStreamEvent({ event: { type: "claudeCodePhase", sessionId: "native-phase", runId: "native-run", phase: "thinking" }, source: { ...source, streamRevision: 3 } });
      expect(native.claudeCodePhase.value).toBeNull();
    } finally { app.unmount(); }
  });

  it("restores multi agent per session, synchronizes it, and forwards it to each run", async () => {
    mocks.loadSession.mockImplementation(async (id: string) => ({
      id, title: id, sessionType: "chat", parentSessionId: null,
      createdAt: 1, updatedAt: 1, messages: [], pendingInputs: [],
      lastMultiAgentEnabled: id === "multi-agent-on" ? true : undefined,
    }));
    mocks.chat.mockImplementation(async (request) => ({ sessionId: request.sessionId, runId: `run-${request.sessionId}` }));
    let first!: ReturnType<typeof useEmbeddedChatSession>;
    let second!: ReturnType<typeof useEmbeddedChatSession>;
    const app = createApp(defineComponent({
      setup() {
        const common = {
          workspaceRef: { checkoutId: "checkout-multi-agent", expectedGeneration: 1 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        };
        first = useEmbeddedChatSession({ ...common, sessionKey: "multi-agent-pane-on", initialSessionId: "multi-agent-on" });
        second = useEmbeddedChatSession({ ...common, sessionKey: "multi-agent-pane-off", initialSessionId: "multi-agent-off" });
        return () => h("div");
      },
    }));
    app.mount(document.createElement("div"));
    try {
      await vi.waitFor(() => expect(first.sessionMultiAgentEnabled.value).toBe(true));
      expect(second.sessionMultiAgentEnabled.value).toBe(false);
      publishSessionExecutionState({ sessionId: "multi-agent-on", modelId: "model-a", effort: "max", fastMode: false, multiAgentEnabled: false });
      expect(first.sessionMultiAgentEnabled.value).toBe(false);
      expect(second.sessionMultiAgentEnabled.value).toBe(false);
      publishSessionExecutionState({ sessionId: "multi-agent-on", modelId: "model-a", effort: "high", fastMode: false, multiAgentEnabled: true });
      await first.compact();
      await second.compact();
      expect(mocks.chat).toHaveBeenCalledWith(expect.objectContaining({ sessionId: "multi-agent-on", multiAgentEnabled: true }));
      expect(mocks.chat).toHaveBeenCalledWith(expect.objectContaining({ sessionId: "multi-agent-off", multiAgentEnabled: false }));
    } finally {
      app.unmount();
    }
  });

  it("starts manual compaction for an idle workbench session without adding a user message", async () => {
    const sessionId = "idle-workbench-compact-session";
    const existingMessage = {
      id: "user-before-compact",
      role: "user" as const,
      content: "Keep this message",
      createdAt: 1,
    };
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Idle compact",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      messages: [existingMessage],
      pendingInputs: [],
    });
    mocks.getSessionUsage.mockResolvedValue(null);
    mocks.chat.mockResolvedValue({ sessionId, runId: "manual-compact-run" });

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "idle-workbench-compact-pane",
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-compact", expectedGeneration: 3 },
          selectedModelId: "model-a",
          selectedAgentId: "unity",
          effort: "high",
          effortSupported: true,
          fastMode: true,
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(session.messages.value).toHaveLength(1));

    await expect(session.compact()).resolves.toBe(true);

    expect(mocks.chat).toHaveBeenCalledWith(expect.objectContaining({
      sessionId,
      text: "",
      mode: "compact",
      images: null,
      assetRefs: null,
      userIntent: null,
    }));
    expect(session.messages.value).toEqual([existingMessage]);
    expect(session.isStreaming.value).toBe(true);

    app.unmount();
  });

  it("queues manual compaction behind the active workbench run", async () => {
    const sessionId = "running-workbench-compact-session";
    const runId = "running-workbench-run";
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Running compact",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      messages: [],
      pendingInputs: [],
      runtime: {
        activeRun: {
          runId,
          sessionId,
          status: "running",
          startedAt: 1,
          updatedAt: 1,
        },
        activeToolCalls: [],
        pendingToolConfirms: [],
        isCompacting: false,
        compactQueued: false,
      },
    });
    mocks.getSessionUsage.mockResolvedValue(null);
    mocks.queueSessionCompact.mockResolvedValue(true);

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "running-workbench-compact-pane",
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-running-compact", expectedGeneration: 5 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(session.isStreaming.value).toBe(true));

    await expect(session.compact()).resolves.toBe(true);

    expect(mocks.queueSessionCompact).toHaveBeenCalledWith(sessionId, runId);
    expect(mocks.chat).not.toHaveBeenCalled();
    expect(session.compactQueued.value).toBe(true);

    publishSessionStreamEvent({
      event: {
        type: "compactStart",
        sessionId,
        runId,
        contextTokens: 100,
        contextLimit: 1_000,
      },
      source: {
        kind: "workspace",
        projectId: "project-running-compact",
        checkoutId: "checkout-running-compact",
        workspaceGeneration: 5,
        streamRevision: 1,
      },
    });
    expect(session.compactQueued.value).toBe(false);
    expect(session.isCompacting.value).toBe(true);

    app.unmount();
  });

  it("queues manual compaction requested while a workbench run is still launching", async () => {
    const sessionId = "launching-workbench-compact-session";
    const runId = "launching-workbench-run";
    let resolveLaunch!: (value: { sessionId: string; runId: string }) => void;
    mocks.chat.mockReturnValue(new Promise((resolve) => {
      resolveLaunch = resolve;
    }));
    mocks.queueSessionCompact.mockResolvedValue(true);

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "launching-workbench-compact-pane",
          workspaceRef: { checkoutId: "checkout-launching-compact", expectedGeneration: 8 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));

    const sendPromise = session.send({ text: "Start the run" });
    await vi.waitFor(() => expect(session.isStreaming.value).toBe(true));
    await expect(session.compact()).resolves.toBe(true);
    expect(mocks.queueSessionCompact).not.toHaveBeenCalled();

    resolveLaunch({ sessionId, runId });
    await sendPromise;
    await vi.waitFor(() => {
      expect(mocks.queueSessionCompact).toHaveBeenCalledWith(sessionId, runId);
    });
    expect(session.compactQueued.value).toBe(true);

    app.unmount();
  });

  it("refreshes the pane after conversation rollback when file undo is unavailable", async () => {
    const sessionId = "conversation-only-rollback-session";
    const targetMessage = {
      id: "assistant-target",
      role: "assistant" as const,
      content: "Keep this answer",
      createdAt: 1,
    };
    const laterMessage = {
      id: "user-later",
      role: "user" as const,
      content: "Remove this turn",
      createdAt: 2,
    };
    const detail = (messages: typeof targetMessage[] | Array<typeof targetMessage | typeof laterMessage>) => ({
      id: sessionId,
      title: "Conversation rollback",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 2,
      messages,
      pendingInputs: [],
    });
    mocks.loadSession
      .mockResolvedValueOnce(detail([targetMessage, laterMessage]))
      .mockResolvedValue(detail([targetMessage]));
    mocks.getSessionUsage.mockResolvedValue(null);
    mocks.undoList.mockResolvedValue([]);
    mocks.rollbackSessionToMessage.mockResolvedValue(detail([targetMessage]));

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "conversation-only-rollback-pane",
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-rollback", expectedGeneration: 4 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(session.messages.value).toHaveLength(2));

    expect(session.undoableMessageIds.value.size).toBe(0);
    await expect(session.rollbackConversation("assistant-target")).resolves.toBe(true);
    expect(mocks.rollbackSessionToMessage).toHaveBeenCalledWith(sessionId, "assistant-target");
    expect(session.messages.value.map((message) => message.id)).toEqual(["assistant-target"]);

    await expect(session.performUndo("assistant-target", {
      force: true,
      acceptDirty: true,
    })).resolves.toBe(true);
    expect(mocks.undoPerform).toHaveBeenCalledWith(
      sessionId,
      "assistant-target",
      true,
      true,
    );

    app.unmount();
  });

  it("keeps the raw Tauri stream listener out of session pane lifecycles", async () => {
    const Root = defineComponent({
      setup() {
        const common = {
          workspaceRef: { checkoutId: "checkout-transport-owner", expectedGeneration: 2 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        };
        useEmbeddedChatSession({ ...common, sessionKey: "transport-pane-a" });
        useEmbeddedChatSession({ ...common, sessionKey: "transport-pane-b" });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await Promise.resolve();
    app.unmount();

    expect(mocks.tauriListen).not.toHaveBeenCalled();
  });

  it("reduces a session event once while two panes observe the same durable session", async () => {
    const sessionId = "shared-session-distribution";
    const runId = "shared-run-distribution";
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Shared session",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      messages: [],
      pendingInputs: [],
      runtime: {
        activeRun: {
          runId,
          sessionId,
          status: "running",
          startedAt: 1,
          updatedAt: 1,
        },
        activeToolCalls: [],
        pendingToolConfirms: [],
        isCompacting: false,
      },
    });
    mocks.getSessionUsage.mockResolvedValue(null);

    let first!: ReturnType<typeof useEmbeddedChatSession>;
    let second!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        const common = {
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-a", expectedGeneration: 3 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        };
        first = useEmbeddedChatSession({ ...common, sessionKey: "pane-a" });
        second = useEmbeddedChatSession({ ...common, sessionKey: "pane-b" });
        return () => h("div");
      },
    });
    const host = document.createElement("div");
    const app = createApp(Root);
    app.mount(host);
    await vi.waitFor(() => {
      expect(first.isStreaming.value).toBe(true);
      expect(second.isStreaming.value).toBe(true);
    });

    expect(first.messages.value).toBe(second.messages.value);
    first.inputText.value = "pane-local draft";
    expect(second.inputText.value).toBe("");

    vi.useFakeTimers();
    const textEvent = {
      type: "textDelta" as const,
      sessionId,
      runId,
      text: "Hello",
    };
    publishSessionStreamEvent({
      event: textEvent,
      source: {
        kind: "workspace",
        projectId: "project-a",
        checkoutId: "checkout-a",
        workspaceGeneration: 3,
        streamRevision: 1,
      },
    });
    await vi.advanceTimersByTimeAsync(100);

    expect(first.streamingText.value).toBe("Hello");
    expect(second.streamingText.value).toBe("Hello");

    publishSessionStreamEvent({
      event: {
        type: "done",
        sessionId,
        runId,
        messageId: "assistant-1",
        fullText: "Hello",
      },
      source: {
        kind: "workspace",
        projectId: "project-a",
        checkoutId: "checkout-a",
        workspaceGeneration: 3,
        streamRevision: 2,
      },
    });

    expect(first.isStreaming.value).toBe(false);
    expect(second.isStreaming.value).toBe(false);
    expect(first.messages.value).toBe(second.messages.value);
    expect(first.messages.value).toEqual([
      expect.objectContaining({
        id: "assistant-1",
        role: "assistant",
        content: "Hello",
      }),
    ]);

    app.unmount();
  });

  it("shows inputs settled after a run ends without adopting the ended run", async () => {
    const sessionId = "settled-input-session";
    const runId = "settled-input-run";
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Settled input",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      messages: [],
      pendingInputs: [],
      runtime: {
        activeRun: { runId, sessionId, status: "running", startedAt: 1, updatedAt: 1 },
        activeToolCalls: [],
        pendingToolConfirms: [],
        isCompacting: false,
      },
    });
    mocks.getSessionUsage.mockResolvedValue(null);

    let pane!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        pane = useEmbeddedChatSession({
          initialSessionId: sessionId,
          sessionKey: "settled-input-pane",
          workspaceRef: { checkoutId: "checkout-a", expectedGeneration: 3 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(pane.isStreaming.value).toBe(true));

    const source = {
      kind: "workspace" as const,
      projectId: "project-a",
      checkoutId: "checkout-a",
      workspaceGeneration: 3,
    };
    const input = {
      id: "steer-1", sessionId, runId, mergeGroupId: "group", status: "delivering",
      delivery: "immediate", text: "late update", displayText: "late update",
      createdAt: 1, updatedAt: 1, revision: 2,
    };
    publishSessionStreamEvent({
      event: { type: "pendingInputQueued", sessionId, runId, input },
      source: { ...source, streamRevision: 1 },
    });
    expect(pane.queuedFollowUp.value?.canEdit).toBe(false);

    publishSessionStreamEvent({
      event: { type: "done", sessionId, runId, messageId: "answer", fullText: "" },
      source: { ...source, streamRevision: 2 },
    });
    publishSessionStreamEvent({
      event: {
        type: "pendingInputQueued",
        sessionId,
        runId,
        input: { ...input, status: "retained", revision: 3 },
      },
      source: { ...source, streamRevision: 3 },
    });

    expect(pane.isStreaming.value).toBe(false);
    expect(pane.currentRunId.value).toBeNull();
    expect(pane.queuedFollowUp.value).toMatchObject({
      displayText: "late update",
      isUndelivered: true,
      canEdit: true,
      canInsert: false,
    });

    app.unmount();
  });

  it("replays a terminal event received while a durable session editor is remounting", async () => {
    const sessionId = "shared-session-remount-gap";
    const runId = "shared-run-remount-gap";
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Remount gap session",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      messages: [{
        id: "user-before-remount",
        role: "user",
        content: "Keep observing this run",
        createdAt: 1,
      }],
      pendingInputs: [],
      runtime: {
        activeRun: {
          runId,
          sessionId,
          status: "running",
          startedAt: 1,
          updatedAt: 1,
        },
        activeToolCalls: [],
        pendingToolConfirms: [],
        isCompacting: false,
      },
    });
    mocks.getSessionUsage.mockResolvedValue(null);

    const common = {
      initialSessionId: sessionId,
      workspaceRef: { checkoutId: "checkout-remount", expectedGeneration: 7 },
      selectedModelId: "model-a",
      buildRequest: (input: string) => ({ text: input }),
    };
    let first!: ReturnType<typeof useEmbeddedChatSession>;
    const FirstRoot = defineComponent({
      setup() {
        first = useEmbeddedChatSession({ ...common, sessionKey: "remount-gap-pane" });
        return () => h("div");
      },
    });
    const firstApp = createApp(FirstRoot);
    firstApp.mount(document.createElement("div"));
    await vi.waitFor(() => expect(first.isStreaming.value).toBe(true));
    firstApp.unmount();

    publishSessionStreamEvent({
      event: {
        type: "done",
        sessionId,
        runId,
        messageId: "assistant-during-remount",
        fullText: "The run finished while the editor was moving.",
      },
      source: {
        kind: "workspace",
        projectId: "project-remount",
        checkoutId: "checkout-remount",
        workspaceGeneration: 7,
        streamRevision: 1,
      },
    });

    let remounted!: ReturnType<typeof useEmbeddedChatSession>;
    const RemountedRoot = defineComponent({
      setup() {
        remounted = useEmbeddedChatSession({ ...common, sessionKey: "remount-gap-pane" });
        return () => h("div");
      },
    });
    const remountedApp = createApp(RemountedRoot);
    remountedApp.mount(document.createElement("div"));

    await vi.waitFor(() => expect(remounted.isStreaming.value).toBe(false));
    expect(remounted.messages.value).toContainEqual(expect.objectContaining({
      id: "assistant-during-remount",
      role: "assistant",
      content: "The run finished while the editor was moving.",
    }));
    expect(mocks.loadSession).toHaveBeenCalledTimes(1);
    remountedApp.unmount();
  });

  it("recovers from a stale run id when a newer run starts", async () => {
    const sessionId = "stale-run-recovery-session";
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Stale run recovery",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      messages: [],
      pendingInputs: [],
      runtime: {
        activeRun: {
          runId: "stale-run",
          sessionId,
          status: "running",
          startedAt: 1,
          updatedAt: 1,
        },
        activeToolCalls: [],
        pendingToolConfirms: [],
        isCompacting: false,
      },
    });
    mocks.getSessionUsage.mockResolvedValue(null);

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "stale-run-recovery-pane",
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-stale-run", expectedGeneration: 11 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(session.isStreaming.value).toBe(true));

    const source = {
      kind: "workspace" as const,
      projectId: "project-stale-run",
      checkoutId: "checkout-stale-run",
      workspaceGeneration: 11,
      streamRevision: 1,
    };
    publishSessionStreamEvent({
      event: { type: "runStart", sessionId, runId: "current-run" },
      source,
    });
    publishSessionStreamEvent({
      event: {
        type: "done",
        sessionId,
        runId: "current-run",
        messageId: "current-run-assistant",
        fullText: "Recovered on the current run.",
      },
      source: { ...source, streamRevision: 2 },
    });

    expect(session.isStreaming.value).toBe(false);
    expect(session.messages.value).toContainEqual(expect.objectContaining({
      id: "current-run-assistant",
      content: "Recovered on the current run.",
    }));
    app.unmount();
  });

  it("refreshes backend state after cancelling an embedded session", async () => {
    const sessionId = "cancel-refresh-session";
    const base = {
      id: sessionId,
      title: "Cancel refresh",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      pendingInputs: [],
    };
    mocks.loadSession
      .mockResolvedValueOnce({
        ...base,
        messages: [],
        runtime: {
          activeRun: {
            runId: "cancel-refresh-run",
            sessionId,
            status: "running",
            startedAt: 1,
            updatedAt: 1,
          },
          activeToolCalls: [],
          pendingToolConfirms: [],
          isCompacting: false,
        },
      })
      .mockResolvedValueOnce({
        ...base,
        messages: [{
          id: "cancelled-assistant",
          role: "assistant",
          content: "Interrupted output",
          createdAt: 2,
        }],
        runtime: null,
      });
    mocks.getSessionUsage.mockResolvedValue(null);
    mocks.cancelChat.mockResolvedValue(undefined);

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "cancel-refresh-pane",
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-cancel-refresh", expectedGeneration: 12 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(session.isStreaming.value).toBe(true));

    await session.cancel();

    expect(mocks.cancelChat).toHaveBeenCalledWith(sessionId);
    expect(session.isCancelling.value).toBe(false);
    expect(session.isStreaming.value).toBe(false);
    expect(session.messages.value).toContainEqual(expect.objectContaining({
      id: "cancelled-assistant",
      content: "Interrupted output",
    }));
    app.unmount();
  });

  it("detaches a reset pane without clearing another pane bound to the same session", async () => {
    const sessionId = "shared-session-reset";
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Previous session",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 1,
      messages: [{
        id: "user-before-reset",
        role: "user",
        content: "Keep this transcript",
        createdAt: 1,
      }],
      pendingInputs: [],
      runtime: null,
    });
    mocks.getSessionUsage.mockResolvedValue(null);

    let first!: ReturnType<typeof useEmbeddedChatSession>;
    let second!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        const common = {
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-reset", expectedGeneration: 4 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        };
        first = useEmbeddedChatSession({ ...common, sessionKey: "reset-pane-a" });
        second = useEmbeddedChatSession({ ...common, sessionKey: "reset-pane-b" });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => {
      expect(first.messages.value).toHaveLength(1);
      expect(second.messages.value).toHaveLength(1);
    });

    first.resetSession();

    expect(first.sessionId.value).toBeNull();
    expect(first.messages.value).toEqual([]);
    expect(second.sessionId.value).toBe(sessionId);
    expect(second.messages.value).toEqual([
      expect.objectContaining({
        id: "user-before-reset",
        content: "Keep this transcript",
      }),
    ]);
    app.unmount();
  });

  it("defers cancellation until launch and keeps a visible prompt out of the composer", async () => {
    let resolveLaunch!: (value: { sessionId: string; runId: string }) => void;
    mocks.chat.mockImplementationOnce(() => new Promise((resolve) => {
      resolveLaunch = resolve;
    }));
    mocks.cancelChat.mockResolvedValue(undefined);

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "cancel-draft-pane",
          workspaceRef: { checkoutId: "checkout-cancel", expectedGeneration: 9 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input, displayText: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));

    session.inputText.value = "restore this prompt";
    const sending = session.send();
    await session.cancel();
    expect(mocks.cancelChat).not.toHaveBeenCalled();

    resolveLaunch({ sessionId: "cancel-session", runId: "cancel-run" });
    await sending;
    expect(mocks.cancelChat).toHaveBeenCalledWith("cancel-session");

    const pendingMessage = session.messages.value.find((message) => message.role === "user");
    publishSessionStreamEvent({
      event: {
        type: "userMessage",
        sessionId: "cancel-session",
        runId: "cancel-run",
        message: {
          id: "cancel-user-persisted",
          role: "user",
          content: "restore this prompt",
          createdAt: 1,
          thinkingSignature: pendingMessage?.thinkingSignature,
        },
      },
      source: {
        kind: "workspace",
        projectId: "project-cancel",
        checkoutId: "checkout-cancel",
        workspaceGeneration: 9,
        streamRevision: 1,
      },
    });

    publishSessionStreamEvent({
      event: {
        type: "cancelled",
        sessionId: "cancel-session",
        runId: "cancel-run",
        messageId: "assistant-cancelled",
        fullText: "partial",
      },
      source: {
        kind: "workspace",
        projectId: "project-cancel",
        checkoutId: "checkout-cancel",
        workspaceGeneration: 9,
        streamRevision: 2,
      },
    });

    expect(session.restoredComposerDraft.value).toBeNull();
    expect(session.messages.value).toContainEqual(expect.objectContaining({
      id: "cancel-user-persisted",
      content: "restore this prompt",
    }));
    app.unmount();
  });

  it("returns a backend-revoked prompt to its composer", async () => {
    mocks.chat.mockResolvedValueOnce({ sessionId: "revoked-session", runId: "revoked-run" });
    const promptText = [
      "revoked prompt",
      "",
      "<locus-local-files>",
      "These are local paths supplied by drag and drop.",
      "- file: `E:/cache/revoked-reference.psd`; type: psd",
      "</locus-local-files>",
    ].join("\n");

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "revoked-draft-pane",
          workspaceRef: { checkoutId: "checkout-revoked", expectedGeneration: 10 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({
            text: promptText,
            displayText: `${input}\n\nrevoked-reference.psd`,
          }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));

    session.inputText.value = "revoked prompt";
    await session.send();
    const pendingMessage = session.messages.value.find((message) => message.role === "user");
    const persistedMessage = {
      id: "revoked-user-persisted",
      role: "user" as const,
      content: "revoked prompt",
      createdAt: 1,
      thinkingSignature: pendingMessage?.thinkingSignature,
    };
    const source = {
      kind: "workspace" as const,
      projectId: "project-revoked",
      checkoutId: "checkout-revoked",
      workspaceGeneration: 10,
      streamRevision: 1,
    };
    publishSessionStreamEvent({
      event: {
        type: "userMessage",
        sessionId: "revoked-session",
        runId: "revoked-run",
        message: persistedMessage,
      },
      source,
    });
    publishSessionStreamEvent({
      event: {
        type: "cancelled",
        sessionId: "revoked-session",
        runId: "revoked-run",
        removedUserMessage: persistedMessage,
      },
      source: { ...source, streamRevision: 2 },
    });

    expect(session.messages.value).toEqual([]);
    expect(session.restoredComposerDraft.value?.text).toBe("revoked prompt");
    expect(session.restoredComposerDraft.value?.localFiles).toEqual([
      expect.objectContaining({
        path: "E:/cache/revoked-reference.psd",
        isDir: false,
        typeLabel: "psd",
      }),
    ]);
    app.unmount();
  });

  it("binds pre-launch events by the session id returned from each concurrent chat", async () => {
    let resolveFirst!: (value: { sessionId: string; runId: string }) => void;
    let resolveSecond!: (value: { sessionId: string; runId: string }) => void;
    const firstLaunch = new Promise<{ sessionId: string; runId: string }>((resolve) => {
      resolveFirst = resolve;
    });
    const secondLaunch = new Promise<{ sessionId: string; runId: string }>((resolve) => {
      resolveSecond = resolve;
    });
    mocks.chat.mockImplementation((request: { text: string }) => (
      request.text === "first" ? firstLaunch : secondLaunch
    ));

    let first!: ReturnType<typeof useEmbeddedChatSession>;
    let second!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        const common = {
          workspaceRef: { checkoutId: "checkout-concurrent", expectedGeneration: 8 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        };
        first = useEmbeddedChatSession({ ...common, sessionKey: "concurrent-pane-a" });
        second = useEmbeddedChatSession({ ...common, sessionKey: "concurrent-pane-b" });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    vi.useFakeTimers();
    first.inputText.value = "first";
    second.inputText.value = "second";
    const firstSend = first.send();
    const secondSend = second.send();

    const source = {
      kind: "workspace" as const,
      projectId: "project-concurrent",
      checkoutId: "checkout-concurrent",
      workspaceGeneration: 8,
      streamRevision: 1,
    };
    publishSessionStreamEvent({
      event: { type: "runStart", sessionId: "session-second", runId: "run-second" },
      source,
    });
    publishSessionStreamEvent({
      event: {
        type: "textDelta",
        sessionId: "session-second",
        runId: "run-second",
        text: "second response",
      },
      source: { ...source, streamRevision: 2 },
    });
    publishSessionStreamEvent({
      event: { type: "runStart", sessionId: "session-first", runId: "run-first" },
      source: { ...source, streamRevision: 3 },
    });
    publishSessionStreamEvent({
      event: {
        type: "textDelta",
        sessionId: "session-first",
        runId: "run-first",
        text: "first response",
      },
      source: { ...source, streamRevision: 4 },
    });

    resolveSecond({ sessionId: "session-second", runId: "run-second" });
    resolveFirst({ sessionId: "session-first", runId: "run-first" });
    await Promise.all([firstSend, secondSend]);
    await vi.advanceTimersByTimeAsync(100);

    expect(first.sessionId.value).toBe("session-first");
    expect(second.sessionId.value).toBe("session-second");
    expect(first.streamingText.value).toBe("first response");
    expect(second.streamingText.value).toBe("second response");
    app.unmount();
  });

  it('shows the saved transcript first and reloads it only when remote history changed', async () => {
    for (const changed of [false, true]) {
      const id = `remote-history-${changed}`;
      let finishSync!: (value: { changed: boolean; warning: null }) => void;
      mocks.synchronizeSessionRemoteHistory.mockReturnValueOnce(new Promise((resolve) => { finishSync = resolve; }));
      const detail = { id, title: 'Remote', sessionType: 'chat', parentSessionId: null, createdAt: 1, updatedAt: 2,
        latestCompletedRunId: null, pendingInputs: [], messages: [{ id: 'saved', role: 'user', content: 'saved', createdAt: 1 }] };
      mocks.loadSession.mockResolvedValue(detail);
      mocks.loadSessionView.mockClear();
      let session!: ReturnType<typeof useEmbeddedChatSession>;
      const app = createApp(defineComponent({ setup() {
        session = useEmbeddedChatSession({ sessionKey: id, initialSessionId: id,
          workspaceRef: { checkoutId: 'remote-checkout', expectedGeneration: 1 },
          selectedModelId: 'model-a', buildRequest: (text: string) => ({ text }) });
        return () => h('div');
      } }));
      app.mount(document.createElement('div'));
      try {
        await vi.waitFor(() => expect(session.messages.value.map((message) => message.id)).toEqual(['saved']));
        expect(mocks.loadSessionView).toHaveBeenCalledTimes(1);
        expect(mocks.loadSessionView).toHaveBeenCalledWith(id, expect.any(Number), false);
        const rendered = session.messages.value;
        mocks.loadSession.mockResolvedValue({ ...detail, messages: [...detail.messages,
          { id: 'remote', role: 'assistant', content: 'remote reply', createdAt: 2 }] });
        finishSync({ changed, warning: null });
        if (changed) {
          await vi.waitFor(() => expect(session.messages.value.map((message) => message.id)).toEqual(['saved', 'remote']));
          expect(mocks.loadSessionView).toHaveBeenCalledTimes(2);
          expect(mocks.loadSessionView).toHaveBeenLastCalledWith(id, expect.any(Number), false);
        } else {
          for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
          expect(mocks.loadSessionView).toHaveBeenCalledTimes(1);
          expect(session.messages.value).toBe(rendered);
        }
      } finally { app.unmount(); }
    }
  });

  it('refreshes saved native history and session tasks in every shared pane without another remote sync', async () => {
    vi.useFakeTimers();
    const id='native-content-refresh';
    const detail={id,title:'Native',sessionType:'chat',parentSessionId:null,createdAt:1,updatedAt:2,latestCompletedRunId:null,pendingInputs:[],messages:[{id:'old',role:'user',content:'old',createdAt:1}]};
    mocks.loadSession.mockResolvedValue(detail);
    let first!:ReturnType<typeof useEmbeddedChatSession>,second!:ReturnType<typeof useEmbeddedChatSession>;
    const app=createApp(defineComponent({setup(){
      const options={initialSessionId:id,workspaceRef:{checkoutId:'native-checkout',expectedGeneration:1},selectedModelId:'model-a',buildRequest:(text:string)=>({text})};
      first=useEmbeddedChatSession({...options,sessionKey:'native-pane-a'});
      second=useEmbeddedChatSession({...options,sessionKey:'native-pane-b'});
      return ()=>h('div');
    }}));
    app.mount(document.createElement('div'));
    try {
      await vi.advanceTimersByTimeAsync(100);
      mocks.loadSessionView.mockClear();
      const source={kind:'workspace' as const,projectId:'project',checkoutId:'native-checkout',workspaceGeneration:1,streamRevision:1};
      publishSessionContentChange({sessionId:id,source:{...source,checkoutId:'unrelated'}});
      expect(mocks.loadSessionView).not.toHaveBeenCalled();
      mocks.loadSession.mockResolvedValue({...detail,messages:[...detail.messages,{id:'recovered',role:'assistant',content:'remote reply',createdAt:2}]});
      mocks.getTodos.mockResolvedValue({latestRunId:null,sessionScoped:true,items:[{content:'remote task',status:'completed',priority:'medium'}]});
      publishSessionContentChange({sessionId:id,source});
      await vi.advanceTimersByTimeAsync(100);
      expect(mocks.loadSessionView).toHaveBeenCalledTimes(1);
      expect(mocks.loadSessionView).toHaveBeenCalledWith(id,expect.any(Number),false);
      expect(first.messages.value.slice(-1)[0]?.content).toBe('remote reply');
      expect(second.messages.value.slice(-1)[0]?.content).toBe('remote reply');
      expect(first.visibleTodos.value[0]?.status).toBe('completed');
      expect(second.visibleTodos.value[0]?.status).toBe('completed');
    } finally {app.unmount();vi.useRealTimers();}
  });

  it.each([false, true])("shows the transcript before slow metadata and preserves newer stream state (%s)", async (streamWhileLoading) => {
    const sessionId = `transcript-before-metadata-${streamWhileLoading}`;
    let finishUsage!: (value: null) => void;
    mocks.getSessionUsage.mockReturnValueOnce(new Promise((resolve) => { finishUsage = resolve; }));
    mocks.getSessionResumeAvailable.mockResolvedValueOnce(true);
    mocks.getSessionPlanState.mockResolvedValueOnce({ active: false, planFilePath: "", planFileExists: false });
    mocks.getTodos.mockResolvedValueOnce({ latestRunId: "completed", items: [
      { content: "Existing todo", status: "pending", priority: "medium" },
    ] });
    mocks.loadSession.mockResolvedValueOnce({
      id: sessionId, title: "Loaded", sessionType: "chat", parentSessionId: null,
      createdAt: 1, updatedAt: 2, latestCompletedRunId: "completed", pendingInputs: [],
      messages: [{ id: "message", role: "user", content: "Visible transcript", createdAt: 1 }],
    });
    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const app = createApp(defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: sessionId, initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-latency", expectedGeneration: 1 },
          selectedModelId: "model-a", buildRequest: (text) => ({ text }),
        });
        return () => h("div");
      },
    }));
    app.mount(document.createElement("div"));
    try {
      await vi.waitFor(() => expect(session.isLoading.value).toBe(false));
      expect(session.messages.value[0]?.content).toBe("Visible transcript");
      expect(session.todos.value).toEqual([]);
      if (streamWhileLoading) {
        const source = { kind: "workspace" as const, projectId: "project-latency",
          checkoutId: "checkout-latency", workspaceGeneration: 1, streamRevision: 1 };
        publishSessionStreamEvent({ event: { type: "runStart", sessionId, runId: "new-run" }, source });
        publishSessionStreamEvent({ event: { type: "textDelta", sessionId, runId: "new-run", text: "New stream" },
          source: { ...source, streamRevision: 2 } });
        publishSessionStreamEvent({ event: { type: "planModeChanged", sessionId, runId: "new-run",
          active: true, planFilePath: "plan/new.md" }, source: { ...source, streamRevision: 3 } });
      }
      finishUsage(null);
      await vi.waitFor(() => expect(session.todos.value).toHaveLength(1));
      expect(session.messages.value[0]?.content).toBe("Visible transcript");
      expect(session.isStreaming.value).toBe(streamWhileLoading);
      expect(session.canResumeInterrupted.value).toBe(!streamWhileLoading);
      expect(session.planModeActive.value).toBe(streamWhileLoading);
      if (streamWhileLoading) await vi.waitFor(() => expect(session.streamingText.value).toBe("New stream"));
    } finally {
      finishUsage(null);
      app.unmount();
    }
  });

  function loadedSession(sessionId: string, content = "Loaded transcript") {
    return {
      id: sessionId, title: "Loaded", sessionType: "chat", parentSessionId: null,
      createdAt: 1, updatedAt: 2, latestCompletedRunId: null, pendingInputs: [],
      messages: [{ id: `${sessionId}-message`, role: "user", content, createdAt: 1 }],
    };
  }

  function mountSession(sessionId: string, workspaceRef = { checkoutId: "checkout-preload", expectedGeneration: 1 }) {
    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const app = createApp(defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: `key-${sessionId}`, initialSessionId: sessionId, workspaceRef,
          selectedModelId: "model-a", buildRequest: (text) => ({ text }),
        });
        return () => h("div");
      },
    }));
    app.mount(document.createElement("div"));
    return { app, session: () => session };
  }

  it("shares one stream error notice with the global chat and marks it already reported to the pane", async () => {
    const id = "queued-input-error-notice";
    mocks.loadSession.mockResolvedValue(loadedSession(id));
    const { app, session } = mountSession(id);
    try {
      await vi.waitFor(() => expect(session().isLoading.value).toBe(false));
      const notices = useNotificationStore();
      // The ordinary Chat listener and embedded pane receive the same event.
      notices.addNotice("error", "unverified history", { code: "chat.stream_failed", operation: "chat" });
      const source = { kind: "workspace" as const, projectId: "project-notice", checkoutId: "checkout-preload", workspaceGeneration: 1, streamRevision: 1 };
      publishSessionStreamEvent({ event: { type: "runStart", sessionId: id, runId: "run-notice" }, source });
      publishSessionStreamEvent({ event: { type: "error", sessionId: id, runId: "run-notice", error: {
        code: "chat.stream_failed", message: "unverified history", retryable: false, severity: "error",
      } }, source });
      await vi.waitFor(() => expect(session().errorMessage.value).toBe("unverified history"));
      expect(session().errorNotificationHandled.value).toBe(true);
      expect(notices.notices.filter(notice => notice.message === "unverified history")).toHaveLength(1);
    } finally { app.unmount(); }
  });

  it("shows and deduplicates nonterminal Claude notices in embedded chat, retaining only the final error on failure", async () => {
    const id = "native-diagnostic-notice";
    mocks.loadSession.mockResolvedValue(loadedSession(id));
    const { app, session } = mountSession(id);
    try {
      await vi.waitFor(() => expect(session().isLoading.value).toBe(false));
      const notices = useNotificationStore();
      const source = { kind: "workspace" as const, projectId: "project-notice", checkoutId: "checkout-preload", workspaceGeneration: 1, streamRevision: 1 };
      publishSessionStreamEvent({ event: { type: "runStart", sessionId: id, runId: "run-notice" }, source });
      const before = [...session().messages.value];
      const event = { type: "claudeCodeNotice" as const, sessionId: id, runId: "run-notice", notice: {
        code: "claude_code.model_fallback", message: "Claude Code: Model unavailable; using configured fallback", retryable: false, severity: "warning" as const,
      } };
      const operation = `claudeCode:${id}:run-notice`;
      // Same notice can arrive through both the ordinary and embedded listener.
      notices.addNotice("warning", event.notice.message, { code: event.notice.code, operation, replaceOperation: true, sticky: true });
      publishSessionStreamEvent({ event: { ...event, runId: "old" }, source });
      publishSessionStreamEvent({ event, source });
      expect(notices.notices.filter(n => n.operation === operation)).toHaveLength(1);
      expect(session().isStreaming.value).toBe(true);
      expect(session().messages.value).toEqual(before);
      expect(session().errorMessage.value).toBeNull();
      publishSessionStreamEvent({ event: { type: "error", sessionId: id, runId: "run-notice", error: {
        code: "chat.stream_failed", message: "Native refusal explanation", retryable: false, severity: "error",
      } }, source });
      expect(notices.notices.filter(n => n.operation === operation)).toHaveLength(0);
      expect(notices.notices.some(n => n.message === "Native refusal explanation" && n.level === "error")).toBe(true);
      expect(session().isStreaming.value).toBe(false);
    } finally { app.unmount(); }
  });

  it("applies metadata that is already loaded together with the transcript", async () => {
    const sessionId = "metadata-first-frame";
    let finishView!: () => void;
    const viewReady = new Promise<void>((resolve) => { finishView = resolve; });
    mocks.loadSession.mockImplementationOnce(async () => {
      await viewReady;
      return loadedSession(sessionId);
    });
    mocks.getTodos.mockResolvedValueOnce({ latestRunId: null, items: [
      { content: "Ready todo", status: "pending", priority: "medium" },
    ] });
    const { app, session } = mountSession(sessionId);
    try {
      const todosWhenTranscriptAppeared: number[] = [];
      watch(() => session().messages.value.length, (length) => {
        if (length > 0) todosWhenTranscriptAppeared.push(session().todos.value.length);
      }, { flush: "post" });
      await vi.waitFor(() => expect(mocks.getTodos).toHaveBeenCalledWith(sessionId));
      await Promise.resolve();
      finishView();
      await vi.waitFor(() => expect(session().isLoading.value).toBe(false));
      expect(todosWhenTranscriptAppeared).toEqual([1]);
    } finally {
      app.unmount();
    }
  });

  it("opens a session from its sidebar preload without loading it again", async () => {
    const sessionId = "preloaded-session";
    const workspaceRef = { checkoutId: "checkout-preload", expectedGeneration: 1 };
    mocks.loadSession.mockResolvedValue(loadedSession(sessionId, "Preloaded transcript"));
    preloadEmbeddedChatSession(sessionId, workspaceRef);
    preloadEmbeddedChatSession(sessionId, workspaceRef);
    expect(mocks.loadSessionView).toHaveBeenCalledTimes(1);
    expect(mocks.getTodos).toHaveBeenCalledTimes(1);
    const { app, session } = mountSession(sessionId, workspaceRef);
    try {
      await vi.waitFor(() => expect(session().isLoading.value).toBe(false));
      expect(session().messages.value[0]?.content).toBe("Preloaded transcript");
      expect(mocks.loadSessionView).toHaveBeenCalledTimes(1);
      expect(mocks.getTodos).toHaveBeenCalledTimes(1);
    } finally {
      app.unmount();
    }
  });

  it("drops a preload when its session receives an event before it is opened", async () => {
    const sessionId = "preload-invalidated-session";
    mocks.loadSession.mockResolvedValue(loadedSession(sessionId));
    preloadEmbeddedChatSession(sessionId, null);
    publishSessionStreamEvent({
      event: { type: "runStart", sessionId, runId: "run-after-preload" },
      source: { kind: "legacy" },
    });
    const { app, session } = mountSession(sessionId);
    try {
      await vi.waitFor(() => expect(session().isLoading.value).toBe(false));
      expect(mocks.loadSessionView).toHaveBeenCalledTimes(2);
    } finally {
      app.unmount();
    }
  });

  it("does not open a session from an expired preload", async () => {
    const sessionId = "preload-expired-session";
    mocks.loadSession.mockResolvedValue(loadedSession(sessionId));
    vi.useFakeTimers({ toFake: ["Date"] });
    preloadEmbeddedChatSession(sessionId, null);
    vi.setSystemTime(Date.now() + 2_500);
    const { app, session } = mountSession(sessionId);
    try {
      await vi.waitFor(() => expect(session().isLoading.value).toBe(false));
      expect(mocks.loadSessionView).toHaveBeenCalledTimes(2);
    } finally {
      app.unmount();
      vi.useRealTimers();
    }
  });

  it("hydrates pane-owned Plan, Resume, Todo and Undo state and accepts synthetic Plan events", async () => {
    const sessionId = "scoped-auxiliary-state-session";
    const workspaceRef = { checkoutId: "checkout-auxiliary", expectedGeneration: 12 };
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Auxiliary state",
      sessionType: "chat",
      parentSessionId: "parent-session",
      latestCompletedRunId: "completed-run",
      createdAt: 1,
      updatedAt: 2,
      messages: [],
      pendingInputs: [],
    });
    mocks.getTodos.mockResolvedValue({
      latestRunId: "completed-run",
      items: [{ content: "Verify pane state", status: "in_progress", priority: "high" }],
    });
    mocks.getSessionResumeAvailable.mockResolvedValue(true);
    mocks.getSessionPlanState.mockResolvedValue({
      active: true,
      planFilePath: "plan/scoped.md",
      planFileExists: true,
    });
    mocks.setSessionPlanMode.mockResolvedValue({
      active: true,
      planFilePath: "plan/new.md",
      planFileExists: true,
    });

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "scoped-auxiliary-state-pane",
          initialSessionId: sessionId,
          workspaceRef,
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(mocks.loadSessionView).toHaveBeenCalledWith(sessionId, expect.any(Number), false));
    await vi.waitFor(() => expect(session.errorMessage.value).toBeNull());
    await vi.waitFor(() => expect(session.parentSessionId.value).toBe("parent-session"));

    expect(session.parentSessionId.value).toBe("parent-session");
    expect(session.planModeActive.value).toBe(true);
    expect(session.canResumeInterrupted.value).toBe(true);
    expect(session.visibleTodos.value).toEqual([
      { content: "Verify pane state", status: "in_progress", priority: "high" },
    ]);

    const source = {
      kind: "workspace" as const,
      projectId: "project-auxiliary",
      checkoutId: workspaceRef.checkoutId,
      workspaceGeneration: workspaceRef.expectedGeneration,
      streamRevision: 1,
    };
    publishSessionStreamEvent({
      event: { type: "runStart", sessionId, runId: "active-pane-run" },
      source,
    });
    publishSessionStreamEvent({
      event: {
        type: "planModeChanged",
        sessionId,
        runId: "synthetic-plan-command",
        active: false,
        planFilePath: null,
      },
      source: { ...source, streamRevision: 2 },
    });
    expect(session.planModeActive.value).toBe(false);

    publishSessionStreamEvent({
      event: {
        type: "undoAvailable",
        sessionId,
        runId: "active-pane-run",
        assistantMessageId: "assistant-undoable",
      },
      source: { ...source, streamRevision: 3 },
    });
    expect(session.undoableMessageIds.value.has("assistant-undoable")).toBe(true);

    publishSessionStreamEvent({
      event: {
        type: "toolCallStart",
        sessionId,
        runId: "active-pane-run",
        toolCallId: "todo-write",
        toolName: "todowrite",
        arguments: JSON.stringify({
          todos: [{ content: "Streamed todo", status: "pending", priority: "medium" }],
        }),
      },
      source: { ...source, streamRevision: 4 },
    });
    publishSessionStreamEvent({
      event: {
        type: "toolCallDone",
        sessionId,
        runId: "active-pane-run",
        toolCallId: "todo-write",
        toolName: "todowrite",
        output: "Todos updated",
        outcome: "done",
      },
      source: { ...source, streamRevision: 5 },
    });
    expect(session.currentTodos.value).toEqual([
      { content: "Streamed todo", status: "pending", priority: "medium" },
    ]);
    publishSessionStreamEvent({
      event: {
        type: "done",
        sessionId,
        runId: "active-pane-run",
        messageId: "assistant-finished",
        fullText: "Finished",
      },
      source: { ...source, streamRevision: 6 },
    });

    await expect(session.setPlanMode(true)).resolves.toBe(true);
    expect(mocks.setSessionPlanMode).toHaveBeenCalledWith(sessionId, true, workspaceRef);
    expect(session.planModeActive.value).toBe(true);
    publishSessionExecutionState({
      sessionId,
      modelId: "runtime-model",
      effort: "low",
      fastMode: true,
    });
    expect(session.sessionModelId.value).toBe("runtime-model");
    expect(session.sessionEffort.value).toBe("low");
    expect(session.sessionFastMode.value).toBe(true);
    app.unmount();
  });

  it("uses the configured Plan model and constrains pane Fast mode by the effective model", async () => {
    mocks.modelDefaults.planModel = "plan-model";
    mocks.availableModels.splice(
      0,
      mocks.availableModels.length,
      {
        id: "model-a",
        name: "Model A",
        provider: "openai_codex",
        contextWindow: 128_000,
        supportedEfforts: ["high"],
      },
      {
        id: "plan-model",
        name: "Plan Model",
        provider: "openrouter",
        contextWindow: 128_000,
        supportedEfforts: ["high"],
      },
    );
    mocks.chat.mockResolvedValue({ sessionId: "plan-model-session", runId: "plan-model-run" });

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "plan-model-pane",
          workspaceRef: { checkoutId: "checkout-plan-model", expectedGeneration: 21 },
          selectedModelId: "model-a",
          selectedAgentId: "unity-agent",
          effort: "high",
          effortSupported: true,
          fastMode: true,
          knowledgeMode: "read_only",
          knowledgeFocus: { docType: "design", path: "Design/combat.md" },
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));

    await session.send({ text: "Create a plan", mode: "plan" });

    expect(mocks.chat).toHaveBeenCalledWith(expect.objectContaining({
      workspaceRef: { checkoutId: "checkout-plan-model", expectedGeneration: 21 },
      model: "plan-model",
      effort: "high",
      fastMode: false,
      knowledgeMode: "read_only",
      knowledgeDocType: "design",
      knowledgeDocPath: "Design/combat.md",
      mode: "plan",
    }));
    expect(session.sessionModelId.value).toBe("plan-model");
    expect(session.sessionFastMode.value).toBe(false);
    expect(session.planModeActive.value).toBe(true);
    app.unmount();
  });

  it("resumes an interrupted pane through the shared hidden-run path", async () => {
    const sessionId = "scoped-resume-session";
    const trailingAssistant = {
      id: "assistant-interrupted",
      role: "assistant" as const,
      content: "",
      createdAt: 1,
      toolCalls: [{
        id: "tool-interrupted",
        name: "read",
        arguments: "{}",
      }],
    };
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Resume",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 2,
      messages: [trailingAssistant],
      pendingInputs: [],
    });
    mocks.getSessionResumeAvailable.mockResolvedValue(true);
    mocks.chat.mockResolvedValue({ sessionId, runId: "resumed-run" });

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "scoped-resume-pane",
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-resume", expectedGeneration: 4 },
          selectedModelId: "model-a",
          knowledgeMode: "disabled",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(session.canResumeInterrupted.value).toBe(true));

    await expect(session.resumeInterrupted()).resolves.toBe(true);

    expect(mocks.chat).toHaveBeenCalledWith(expect.objectContaining({
      sessionId,
      workspaceRef: { checkoutId: "checkout-resume", expectedGeneration: 4 },
      text: "",
      resume: true,
      mode: "build",
      knowledgeMode: "disabled",
    }));
    expect(session.messages.value.filter((message) => message.role === "user")).toEqual([]);
    expect(session.messages.value).toContainEqual(expect.objectContaining({
      id: "synthetic_tool_result:assistant-interrupted:tool-interrupted",
      role: "tool",
      toolCallId: "tool-interrupted",
    }));
    expect(session.canResumeInterrupted.value).toBe(false);
    expect(session.isStreaming.value).toBe(true);
    app.unmount();
  });

  it("loads paged history and turn previews for the pane session only", async () => {
    const sessionId = "scoped-history-session";
    mocks.loadSessionView.mockResolvedValueOnce({
      session: {
        id: sessionId,
        title: "History",
        sessionType: "chat",
        parentSessionId: null,
        createdAt: 1,
        updatedAt: 2,
        messages: [{ id: "message-new", role: "assistant", content: "new", createdAt: 2 }],
        pendingInputs: [],
      },
      userMessageIds: ["message-old"],
      oldestMessageRowId: 20,
      hasMoreHistory: true,
    });
    mocks.loadSessionMessagePage.mockResolvedValueOnce({
      messages: [{ id: "message-old", role: "user", content: "old", createdAt: 1 }],
      oldestMessageRowId: 10,
      hasMoreHistory: false,
    });
    mocks.loadSessionTurnPreview.mockResolvedValueOnce({
      messageId: "message-old",
      prompt: "old",
      response: "new",
    });

    let session!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        session = useEmbeddedChatSession({
          sessionKey: "scoped-history-pane",
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-history", expectedGeneration: 7 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(session.sessionHistoryHasMore.value).toBe(true));

    await expect(session.loadSessionTurnPreview("message-old")).resolves.toEqual({
      messageId: "message-old",
      prompt: "old",
      response: "new",
    });
    await expect(session.loadSessionHistoryThroughMessage("message-old")).resolves.toBe(true);

    expect(mocks.loadSessionTurnPreview).toHaveBeenCalledWith(sessionId, "message-old");
    expect(mocks.loadSessionMessagePage).toHaveBeenCalledWith(sessionId, 20, expect.any(Number));
    expect(session.messages.value.map((message) => message.id)).toEqual([
      "message-old",
      "message-new",
    ]);
    expect(session.sessionHistoryHasMore.value).toBe(false);
    expect(session.sessionUserMessageIds.value).toEqual(["message-old"]);
    app.unmount();
  });

  it("applies one async task update to shared durable state observed by two panes", async () => {
    const sessionId = "shared-async-task-session";
    mocks.loadSession.mockResolvedValue({
      id: sessionId,
      title: "Async task",
      sessionType: "chat",
      parentSessionId: null,
      createdAt: 1,
      updatedAt: 2,
      messages: [{
        id: "assistant-async",
        role: "assistant",
        content: "",
        createdAt: 1,
        toolCalls: [{ id: "tool-async", name: "background_task", arguments: "{}" }],
      }],
      pendingInputs: [],
    });

    let first!: ReturnType<typeof useEmbeddedChatSession>;
    let second!: ReturnType<typeof useEmbeddedChatSession>;
    const Root = defineComponent({
      setup() {
        const common = {
          initialSessionId: sessionId,
          workspaceRef: { checkoutId: "checkout-async", expectedGeneration: 2 },
          selectedModelId: "model-a",
          buildRequest: (input: string) => ({ text: input }),
        };
        first = useEmbeddedChatSession({ ...common, sessionKey: "async-pane-a" });
        second = useEmbeddedChatSession({ ...common, sessionKey: "async-pane-b" });
        return () => h("div");
      },
    });
    const app = createApp(Root);
    app.mount(document.createElement("div"));
    await vi.waitFor(() => expect(first.messages.value).toHaveLength(1));

    publishSessionAsyncTaskUpdate({
      sessionId,
      assistantMessageId: "assistant-async",
      toolCallId: "tool-async",
      taskId: "task-async",
      toolName: "background_task",
      status: "completed",
      output: "Async result",
    });

    expect(first.messages.value).toBe(second.messages.value);
    expect(first.messages.value[0]?.toolCalls?.[0]).toEqual(expect.objectContaining({
      id: "tool-async",
      outcome: "done",
      recordedOutput: "Async result",
    }));
    app.unmount();
  });
});
