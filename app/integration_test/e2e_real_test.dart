// Real end-to-end test: Linux desktop app + running agentdock-daemon + real kiro-cli.
// Run: flutter test integration_test/e2e_real_test.dart -d linux
// Needs the daemon on ws://127.0.0.1:7820/ws. Uses a new session in spike/workdir.

import 'dart:io';

import 'package:agentdock_app/app_state.dart';
import 'package:agentdock_app/daemon_client.dart';
import 'package:agentdock_app/main.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

const workdir = '/home/mi/Project/AgentDock/spike/workdir';

Future<void> waitFor(WidgetTester t, bool Function() cond, {int seconds = 180, String what = ''}) async {
  final end = DateTime.now().add(Duration(seconds: seconds));
  while (!cond()) {
    if (DateTime.now().isAfter(end)) fail('timed out waiting for $what');
    await t.pump(const Duration(milliseconds: 300));
  }
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('real daemon: new session → prompt → allow in UI → file created → todo', (t) async {
    final marker = File('$workdir/s5_e2e_marker.txt');
    if (marker.existsSync()) marker.deleteSync();

    final client = DaemonClient(Uri.parse(daemonUrl));
    final state = AppState(client);
    await t.pumpWidget(AgentDockApp(state: state));
    await client.connect();
    await waitFor(t, () => state.conn == ConnState.connected, seconds: 10, what: 'connect');
    await t.pumpAndSettle();

    // new session in spike/workdir (folder picker UI is covered by e2e_takeover_test)
    await state.newSession(workdir);
    await waitFor(t, () => state.currentChat.value != null, seconds: 90, what: 'session.new');
    final sid = state.currentChat.value!;
    await t.pumpAndSettle();

    await t.enterText(find.byKey(const ValueKey('composer')),
        '请调用 shell 工具执行命令：touch ${marker.path}，成功后在回复最后单独一行输出：【待办已确认】检查 S5 端到端标记文件\n只调用这一次工具。');
    await t.tap(find.byKey(const ValueKey('send')));
    await waitFor(t, () => state.approvals.isNotEmpty, what: 'permission.request');
    await t.pumpAndSettle();
    final ap = state.approvals.values.first;
    expect('${ap['rawInput']?['command']}', contains(marker.path), reason: 'card shows the exact command');
    expect(marker.existsSync(), isFalse, reason: 'nothing runs before approval');

    // click "允许一次" on the card inside the chat
    await t.tap(find.byKey(ValueKey('opt-allow_once-${ap['approvalId']}')).first);
    await waitFor(t, () => !state.chats[sid]!.promptRunning, what: 'prompt.done');
    await t.pumpAndSettle();
    expect(marker.existsSync(), isTrue, reason: 'approved command really ran');
    expect(find.textContaining('已处理：允许一次'), findsOneWidget);

    // the todo scanner (3s) imports the confirmed marker
    await waitFor(t, () => state.todos.any((x) => x['title'] == '检查 S5 端到端标记文件' && x['sessionId'] == sid),
        seconds: 20, what: 'todo import');
    state.navIndex.value = 3;
    await t.pumpAndSettle();
    expect(find.text('检查 S5 端到端标记文件'), findsOneWidget);

    // cleanup: delete the todo, detach the session
    final todo = state.todos.firstWhere((x) => x['sessionId'] == sid);
    await state.deleteTodo(todo['id'] as int);
    await state.detach(sid);
    await t.pumpAndSettle();
    marker.deleteSync();
    stdout.writeln('E2E_OK session=$sid');
  });
}
