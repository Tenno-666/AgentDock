import 'dart:async';
import 'dart:convert';

import 'package:agentdock_app/app_state.dart';
import 'package:agentdock_app/daemon_client.dart';
import 'package:agentdock_app/main.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:stream_channel/stream_channel.dart';
import 'package:web_socket_channel/web_socket_channel.dart';

const sid = 'aaaaaaaa-0000-0000-0000-00000000000a';

/// In-memory WebSocketChannel: app writes to [sent], fake daemon pushes via [push].
class FakeChannel with StreamChannelMixin<dynamic> implements WebSocketChannel {
  FakeChannel(this.onRequest);
  final void Function(FakeChannel ch, Map<String, dynamic> req) onRequest;
  final _incoming = StreamController<dynamic>();
  final sent = <Map<String, dynamic>>[];
  late final _out = StreamController<dynamic>()
    ..stream.listen((d) {
      final m = jsonDecode(d as String) as Map<String, dynamic>;
      sent.add(m);
      onRequest(this, m);
    });

  void push(Map<String, dynamic> m) => _incoming.add(jsonEncode(m));
  void reply(Map<String, dynamic> req, Map<String, dynamic> body) =>
      push({...body, 'id': req['id'], 'type': '${req['type']}.result'});

  @override
  Stream<dynamic> get stream => _incoming.stream;
  @override
  WebSocketSink get sink => _Sink(_out.sink);
  @override
  Future<void> get ready => Future.value();
  @override
  String? get protocol => null;
  @override
  int? get closeCode => null;
  @override
  String? get closeReason => null;
}

class _Sink implements WebSocketSink {
  _Sink(this._s);
  final StreamSink<dynamic> _s;
  @override
  void add(dynamic data) => _s.add(data);
  @override
  void addError(Object error, [StackTrace? st]) => _s.addError(error, st);
  @override
  Future<void> addStream(Stream<dynamic> s) => _s.addStream(s);
  @override
  Future<void> close([int? code, String? reason]) => _s.close();
  @override
  Future<void> get done => _s.done;
}

class FakeDaemon {
  final todos = <Map<String, dynamic>>[];
  late FakeChannel ch;

  void handle(FakeChannel c, Map<String, dynamic> r) {
    switch (r['type']) {
      case 'sessions.list':
        c.reply(r, {
          'sessions': [
            {'sessionId': sid, 'title': '测试会话', 'cwd': '/home/mi/w', 'tmuxPane': null, 'pid': 1},
          ],
        });
      case 'attached.list':
        c.reply(r, {'sessions': []});
      case 'permission.list':
        c.reply(r, {'approvals': []});
      case 'todo.list':
        c.reply(r, {'todos': todos});
      case 'session.attach':
        // history replay arrives before the result, as in the real daemon
        c.push({'type': 'message.chunk', 'sessionId': sid, 'replay': true, 'role': 'user', 'text': '旧问题'});
        c.push({'type': 'message.chunk', 'sessionId': sid, 'replay': true, 'role': 'assistant', 'text': '旧回答'});
        c.reply(r, {'sessionId': sid, 'cwd': '/home/mi/w', 'alreadyAttached': false});
      case 'prompt.send':
        c.reply(r, {'sessionId': sid, 'accepted': true});
        c.push({'type': 'prompt.started', 'sessionId': sid, 'text': r['text']});
        c.push({'type': 'message.chunk', 'sessionId': sid, 'replay': false, 'role': 'assistant', 'text': '好的，'});
        c.push({'type': 'message.chunk', 'sessionId': sid, 'replay': false, 'role': 'assistant', 'text': '我来执行'});
        c.push({'type': 'tool.call', 'sessionId': sid, 'phase': 'start', 'toolCallId': 't1', 'title': 'Running: touch /tmp/x', 'rawInput': {'command': 'touch /tmp/x'}});
        c.push({
          'type': 'permission.request', 'approvalId': 'ap-1', 'sessionId': sid, 'toolCallId': 't1',
          'title': 'Running: touch /tmp/x', 'kind': 'execute', 'toolName': 'shell',
          'rawInput': {'command': 'touch /tmp/x'}, 'inputUnknown': false,
          'options': [
            {'optionId': 'allow_once', 'kind': 'allow_once'},
            {'optionId': 'allow_always', 'kind': 'allow_always'},
            {'optionId': 'reject_once', 'kind': 'reject_once'},
          ],
          'createdAtMs': 1,
        });
      case 'permission.reply':
        c.reply(r, {'approvalId': r['approvalId'], 'optionId': r['optionId']});
        c.push({'type': 'permission.resolved', 'approvalId': r['approvalId'], 'sessionId': sid, 'optionId': r['optionId'], 'outcome': 'selected', 'reason': 'user'});
        c.push({'type': 'tool.call', 'sessionId': sid, 'phase': 'update', 'toolCallId': 't1', 'status': 'completed'});
        c.push({'type': 'message.chunk', 'sessionId': sid, 'replay': false, 'role': 'assistant', 'text': '\n【待办已确认】补测试'});
        todos.add({'id': 1, 'title': '补测试', 'status': 'open', 'source': 'auto', 'agent': 'kiro', 'sessionId': sid});
        c.push({'type': 'todo.changed', 'action': 'added', 'todo': todos.last});
        c.push({'type': 'prompt.done', 'sessionId': sid, 'stopReason': 'end_turn'});
      case 'todo.update':
        todos.first['status'] = r['status'];
        c.reply(r, {'todo': todos.first});
        c.push({'type': 'todo.changed', 'action': 'updated', 'todo': todos.first});
      default:
        c.push({'id': r['id'], 'type': 'error', 'code': 'BAD_REQUEST', 'message': 'unknown'});
    }
  }
}

void main() {
  testWidgets('attach → chat → approve → todo end to end', (tester) async {
    tester.view.physicalSize = const Size(1280, 900);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);

    final daemon = FakeDaemon();
    final client = DaemonClient(Uri.parse('ws://fake'), factory: (_) => daemon.ch = FakeChannel(daemon.handle));
    final state = AppState(client);
    await client.connect();
    await tester.pumpWidget(AgentDockApp(state: state));
    await tester.pumpAndSettle();

    // sessions page lists the active session
    expect(find.text('测试会话'), findsOneWidget);
    expect(find.textContaining('其他进程中'), findsOneWidget);

    await tester.tap(find.byKey(const ValueKey('attach-$sid')));
    await tester.pumpAndSettle();
    // switched to chat tab with replayed history
    expect(state.navIndex.value, 1);
    expect(find.text('旧问题'), findsOneWidget);
    expect(find.text('旧回答'), findsOneWidget);

    await tester.enterText(find.byKey(const ValueKey('composer')), '帮我 touch 一下\n第二行');
    await tester.tap(find.byKey(const ValueKey('send')));
    await tester.pumpAndSettle();
    final sentPrompt = daemon.ch.sent.firstWhere((m) => m['type'] == 'prompt.send');
    expect(sentPrompt['text'], '帮我 touch 一下\n第二行', reason: 'multi-line text is sent as one message');
    expect(find.text('好的，我来执行'), findsOneWidget, reason: 'chunks are merged');
    // approval card shows the exact command
    expect(find.textContaining('"command": "touch /tmp/x"'), findsWidgets);
    expect(state.approvals.length, 1);

    await tester.tap(find.byKey(const ValueKey('opt-allow_once-ap-1')).first);
    await tester.pumpAndSettle();
    final reply = daemon.ch.sent.firstWhere((m) => m['type'] == 'permission.reply');
    expect((reply['approvalId'], reply['optionId']), ('ap-1', 'allow_once'));
    expect(state.approvals, isEmpty);
    expect(find.textContaining('已处理：允许一次'), findsOneWidget);
    expect(state.chats[sid]!.promptRunning, isFalse);

    // todo pushed by daemon appears on the todo page
    state.navIndex.value = 3;
    await tester.pumpAndSettle();
    expect(find.text('补测试'), findsOneWidget);
    await tester.tap(find.byType(Checkbox).first);
    await tester.pumpAndSettle();
    final upd = daemon.ch.sent.firstWhere((m) => m['type'] == 'todo.update');
    expect((upd['todoId'], upd['status']), (1, 'done'));
  });

  testWidgets('allow_always needs a second confirmation', (tester) async {
    tester.view.physicalSize = const Size(1280, 900);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    final daemon = FakeDaemon();
    final client = DaemonClient(Uri.parse('ws://fake'), factory: (_) => daemon.ch = FakeChannel(daemon.handle));
    final state = AppState(client);
    await client.connect();
    await tester.pumpWidget(AgentDockApp(state: state));
    await tester.pumpAndSettle();
    await tester.tap(find.byKey(const ValueKey('attach-$sid')));
    await tester.pumpAndSettle();
    await tester.enterText(find.byKey(const ValueKey('composer')), 'x');
    await tester.tap(find.byKey(const ValueKey('send')));
    await tester.pumpAndSettle();

    await tester.tap(find.byKey(const ValueKey('opt-allow_always-ap-1')).first);
    await tester.pumpAndSettle();
    expect(find.text('确认"总是允许"？'), findsOneWidget);
    await tester.tap(find.text('取消'));
    await tester.pumpAndSettle();
    expect(daemon.ch.sent.where((m) => m['type'] == 'permission.reply'), isEmpty, reason: 'cancel sends nothing');
    expect(state.approvals.length, 1);
  });

  testWidgets('narrow layout uses bottom navigation and shows disconnected banner', (tester) async {
    tester.view.physicalSize = const Size(400, 800);
    tester.view.devicePixelRatio = 1;
    addTearDown(tester.view.reset);
    final client = DaemonClient(Uri.parse('ws://fake'), factory: (_) => throw StateError('no daemon'));
    final state = AppState(client);
    await tester.pumpWidget(AgentDockApp(state: state));
    await client.connect();
    await tester.pump();
    expect(find.byType(NavigationBar), findsOneWidget);
    expect(find.textContaining('未连接 daemon'), findsOneWidget);
    await client.close();
  });
}
