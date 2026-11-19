import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';

import 'daemon_client.dart';

typedef Json = Map<String, dynamic>;

enum ChatKind { user, assistant, thought, tool, approval, notice }

/// One row in a chat transcript.
class ChatItem {
  ChatItem(this.kind, {this.text = '', this.replay = false, this.data});
  final ChatKind kind;
  String text;
  final bool replay;

  /// tool: last tool.call event; approval: permission.request event.
  Json? data;

  /// approval only: null while pending, otherwise the chosen option / outcome.
  String? resolution;
}

class ChatSession {
  ChatSession(this.sessionId, this.cwd);
  final String sessionId;
  final String cwd;
  /// tmux pane label the session was taken over from (hand-back target).
  String? originPane;
  final items = <ChatItem>[];
  final _tools = <String, ChatItem>{};
  bool promptRunning = false;
  bool closed = false;

  void addChunk(ChatKind kind, String text, bool replay) {
    if (text.isEmpty) return;
    final last = items.isEmpty ? null : items.last;
    if (last != null && last.kind == kind && last.replay == replay &&
        kind != ChatKind.user) {
      last.text += text;
    } else {
      items.add(ChatItem(kind, text: text, replay: replay));
    }
  }

  void addTool(Json ev) {
    final id = '${ev['toolCallId']}';
    final existing = _tools[id];
    if (existing == null) {
      final item = ChatItem(ChatKind.tool, replay: ev['replay'] == true, data: ev);
      _tools[id] = item;
      items.add(item);
      return;
    }
    // merge: later updates may omit title/rawInput
    final merged = Json.of(existing.data ?? {});
    ev.forEach((k, v) {
      if (v != null && !(v is String && v.isEmpty)) merged[k] = v;
    });
    existing.data = merged;
  }
}

String prettyJson(Object? v) =>
    v == null ? '' : const JsonEncoder.withIndent('  ').convert(v);

/// Client-side model of the daemon. Widgets listen to it.
class AppState extends ChangeNotifier {
  AppState(this.client) {
    _subs.add(client.events.listen(_onEvent));
    _subs.add(client.onConnected.listen((_) => refreshAll()));
    client.state.addListener(notifyListeners);
  }

  final DaemonClient client;
  final _subs = <StreamSubscription<dynamic>>[];

  List<Json> activeSessions = [];
  List<Json> recentSessions = [];
  final chats = <String, ChatSession>{}; // attached sessions, insertion order
  final approvals = <String, Json>{}; // approvalId -> permission.request
  List<Json> todos = [];
  String todoFilter = 'open';
  String? lastError;

  /// Set by UI to jump to a tab / session.
  final ValueNotifier<int> navIndex = ValueNotifier(0);
  final ValueNotifier<String?> currentChat = ValueNotifier(null);

  ConnState get conn => client.state.value;

  Future<T?> _guard<T>(Future<T> Function() f) async {
    try {
      final r = await f();
      lastError = null;
      return r;
    } on DaemonError catch (e) {
      lastError = e.message;
      notifyListeners();
      rethrow;
    }
  }

  Future<void> refreshAll() async {
    try {
      await Future.wait([refreshSessions(), _refreshAttached(), _refreshApprovals(), refreshTodos(), refreshRecent()]);
    } on DaemonError {
      // shown via lastError; next reconnect retries
    }
  }

  Future<void> refreshSessions() async {
    final r = await _guard(() => client.request('sessions.list'));
    activeSessions = List<Json>.from(r?['sessions'] ?? const []);
    notifyListeners();
  }

  Future<void> refreshRecent() async {
    final r = await _guard(() => client.request('sessions.recent', {'limit': 30}));
    recentSessions = List<Json>.from(r?['sessions'] ?? const []);
    notifyListeners();
  }

  Future<void> _refreshAttached() async {
    final r = await _guard(() => client.request('attached.list'));
    final list = List<Json>.from(r?['sessions'] ?? const []);
    final ids = list.map((s) => '${s['sessionId']}').toSet();
    for (final s in list) {
      final c = chats.putIfAbsent('${s['sessionId']}', () => ChatSession('${s['sessionId']}', '${s['cwd']}'));
      c.promptRunning = s['promptRunning'] == true;
      c.closed = false;
      c.originPane = s['originPane'] as String?;
    }
    for (final c in chats.values) {
      if (!ids.contains(c.sessionId)) c.closed = true;
    }
    notifyListeners();
  }

  Future<void> _refreshApprovals() async {
    final r = await _guard(() => client.request('permission.list'));
    approvals
      ..clear()
      ..addEntries(List<Json>.from(r?['approvals'] ?? const [])
          .map((a) => MapEntry('${a['approvalId']}', a)));
    notifyListeners();
  }

  Future<void> refreshTodos() async {
    final r = await _guard(() => client.request('todo.list', {'status': todoFilter}));
    todos = List<Json>.from(r?['todos'] ?? const []);
    notifyListeners();
  }

  // ------------------------------------------------------------ sessions

  ChatSession _chatFor(String sid, [String cwd = '']) =>
      chats.putIfAbsent(sid, () => ChatSession(sid, cwd));

  /// [takeover]: quit the session in its tmux pane first (daemon refuses if busy).
  Future<void> attach(String sessionId, {bool takeover = false}) async {
    // replay events arrive before the result; create the chat first
    final c = _chatFor(sessionId);
    c.items.clear();
    c._tools.clear();
    c.closed = false;
    final Json? r;
    try {
      // takeover waits up to 15s for the terminal to quit, then loads (<=2x60s)
      r = await _guard(() => client.request('session.attach', {'sessionId': sessionId, 'takeover': takeover},
          const Duration(seconds: 160)));
    } on DaemonError {
      if (c.items.isEmpty) chats.remove(sessionId);
      notifyListeners();
      rethrow;
    }
    c.originPane = r?['tookOverFrom'] as String?;
    _openChat(sessionId);
  }

  Future<void> newSession(String cwd) async {
    final r = await _guard(() => client.request('session.new', {'cwd': cwd}));
    final sid = '${r!['sessionId']}';
    _chatFor(sid, '${r['cwd']}');
    _openChat(sid);
  }

  /// Returns the daemon reply (handedBack / pane / handbackError).
  Future<Json?> detach(String sessionId, {bool handback = false}) async {
    final r = await _guard(() => client.request('session.detach', {'sessionId': sessionId, 'handback': handback}));
    chats.remove(sessionId);
    if (currentChat.value == sessionId) {
      currentChat.value = chats.keys.isEmpty ? null : chats.keys.first;
    }
    notifyListeners();
    unawaited(refreshSessions());
    unawaited(refreshRecent().catchError((_) {}));
    return r;
  }

  void _openChat(String sid) {
    currentChat.value = sid;
    navIndex.value = 1;
    notifyListeners();
    unawaited(refreshSessions());
  }

  Future<Json?> viewHistory(String sessionId) =>
      _guard(() => client.request('session.view', {'sessionId': sessionId, 'limit': 5000}));

  Future<void> sendPrompt(String sessionId, String text) async {
    final c = chats[sessionId];
    if (c == null) return;
    await _guard(() => client.request('prompt.send', {'sessionId': sessionId, 'text': text}));
  }

  Future<void> cancelPrompt(String sessionId) =>
      _guard(() => client.request('prompt.cancel', {'sessionId': sessionId}));

  // ------------------------------------------------------------ folders (on the computer)

  Future<Json?> fsList(String? path, {bool showHidden = false}) async {
    final r = await _guard(() => client.request('fs.list', {'path': ?path, 'showHidden': showHidden}));
    return r?['listing'] as Json?;
  }

  Future<void> fsMkdir(String parent, String name) =>
      _guard(() => client.request('fs.mkdir', {'parent': parent, 'name': name}));

  Future<void> fsRename(String path, String newName, {bool force = false}) =>
      _guard(() => client.request('fs.rename', {'path': path, 'newName': newName, 'force': force}));

  // ------------------------------------------------------------ approvals

  Future<void> replyApproval(String approvalId, String optionId) async {
    try {
      await _guard(() => client.request('permission.reply', {'approvalId': approvalId, 'optionId': optionId}));
    } on DaemonError catch (e) {
      if (e.code == 'APPROVAL_NOT_FOUND') {
        approvals.remove(approvalId);
        notifyListeners();
      }
      rethrow;
    }
  }

  // ------------------------------------------------------------ todos

  Future<void> setTodoFilter(String f) async {
    todoFilter = f;
    await refreshTodos();
  }

  Future<void> addTodo(String title, {String? priority, String? dueDate}) async {
    await _guard(() => client.request('todo.add', {
      'title': title,
      'priority': ?priority,
      'dueDate': ?dueDate,
    }));
  }

  Future<void> setTodoDone(int id, bool done) async {
    await _guard(() => client.request('todo.update', {'todoId': id, 'status': done ? 'done' : 'open'}));
  }

  Future<void> deleteTodo(int id) async {
    await _guard(() => client.request('todo.delete', {'todoId': id}));
  }

  int get openTodoCount =>
      todoFilter == 'open' ? todos.length : todos.where((t) => t['status'] == 'open').length;

  // ------------------------------------------------------------ events

  void _onEvent(Json ev) {
    final sid = ev['sessionId'] as String?;
    switch (ev['type']) {
      case 'message.chunk':
        if (sid == null) break;
        final kind = switch (ev['role']) {
          'user' => ChatKind.user,
          'thought' => ChatKind.thought,
          _ => ChatKind.assistant,
        };
        _chatFor(sid).addChunk(kind, '${ev['text'] ?? ''}', ev['replay'] == true);
      case 'tool.call':
        if (sid != null) _chatFor(sid).addTool(ev);
      case 'prompt.started':
        if (sid == null) break;
        final c = _chatFor(sid);
        c.promptRunning = true;
        c.items.add(ChatItem(ChatKind.user, text: '${ev['text'] ?? ''}'));
      case 'prompt.done':
        if (sid == null) break;
        final c = _chatFor(sid);
        c.promptRunning = false;
        final err = ev['error'];
        if (err != null) {
          c.items.add(ChatItem(ChatKind.notice, text: '本轮出错：$err'));
        } else if (ev['stopReason'] == 'cancelled') {
          c.items.add(ChatItem(ChatKind.notice, text: '已取消'));
        }
      case 'permission.request':
        approvals['${ev['approvalId']}'] = ev;
        if (sid != null) _chatFor(sid).items.add(ChatItem(ChatKind.approval, data: ev));
      case 'permission.resolved':
        final id = '${ev['approvalId']}';
        approvals.remove(id);
        for (final c in chats.values) {
          for (final it in c.items) {
            if (it.kind == ChatKind.approval && it.data?['approvalId'] == id) {
              it.resolution = '${ev['optionId'] ?? ev['outcome']}';
            }
          }
        }
      case 'session.attached':
        if (sid != null) _chatFor(sid, '${ev['cwd'] ?? ''}').closed = false;
      case 'session.closed':
        final c = sid == null ? null : chats[sid];
        if (c != null) {
          c.closed = true;
          c.promptRunning = false;
          c.items.add(ChatItem(ChatKind.notice,
              text: ev['reason'] == 'agent_exited' ? 'AI 进程已退出，会话已关闭' : '会话已断开'));
        }
      case 'todo.changed':
        unawaited(refreshTodos().catchError((_) {}));
      case 'events.lagged':
        unawaited(refreshAll());
    }
    notifyListeners();
  }

  @override
  void dispose() {
    for (final s in _subs) {
      s.cancel();
    }
    client.state.removeListener(notifyListeners);
    super.dispose();
  }
}
