// Real end-to-end: Linux app + running daemon + kiro-cli in an isolated tmux server.
// Run: flutter test integration_test/e2e_takeover_test.dart -d linux
// Uses spike test session 622a6bf6 and folders under spike/workdir only.

import 'dart:io';

import 'package:agentdock_app/app_state.dart';
import 'package:agentdock_app/daemon_client.dart';
import 'package:agentdock_app/main.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';
import 'package:integration_test/integration_test.dart';

const sid = '622a6bf6-e6ce-40b5-b842-a5df10415a4c';
const workdir = '/home/mi/Project/AgentDock/spike/workdir';
final lock = File('${Platform.environment['HOME']}/.kiro/sessions/cli/$sid.lock');

Future<void> tmux(List<String> args) async {
  final r = await Process.run('tmux', ['-L', 'agentdock', ...args]);
  if (r.exitCode != 0) throw StateError('tmux $args: ${r.stderr}');
}

Future<void> waitFor(WidgetTester t, bool Function() cond, {int seconds = 120, String what = ''}) async {
  final end = DateTime.now().add(Duration(seconds: seconds));
  while (!cond()) {
    if (DateTime.now().isAfter(end)) fail('timed out waiting for $what');
    await t.pump(const Duration(milliseconds: 300));
  }
}

Future<void> tapAndSettle(WidgetTester t, Finder f) async {
  await t.tap(f);
  await t.pumpAndSettle();
}

void main() {
  IntegrationTestWidgetsFlutterBinding.ensureInitialized();

  testWidgets('takeover from tmux, hand back, then new session via folder picker', (t) async {
    // terminal side: bash pane running the test session
    await Process.run('tmux', ['-L', 'agentdock', 'kill-server']);
    await tmux(['new-session', '-d', '-s', 'e2e', '-x', '160', '-y', '40', '-c', workdir, '/bin/bash']);
    await tmux(['send-keys', '-t', 'e2e', 'kiro-cli --resume-id $sid', 'Enter']);
    addTearDown(() => Process.run('tmux', ['-L', 'agentdock', 'kill-server']));

    final client = DaemonClient(Uri.parse(daemonUrl));
    final state = AppState(client);
    await t.pumpWidget(AgentDockApp(state: state));
    await client.connect();
    await waitFor(t, () => state.conn == ConnState.connected, seconds: 10, what: 'connect');
    await waitFor(t, () => lock.existsSync(), seconds: 60, what: 'terminal session lock');
    await Future<void>.delayed(const Duration(seconds: 3));
    await state.refreshSessions();
    await t.pumpAndSettle();
    expect(state.activeSessions.firstWhere((s) => s['sessionId'] == sid)['tmuxPane'], contains('-L agentdock'));

    // one-click takeover
    await tapAndSettle(t, find.byKey(const ValueKey('attach-$sid')));
    await t.tap(find.byKey(const ValueKey('confirm-takeover')));
    await waitFor(t, () => state.currentChat.value == sid, what: 'attach');
    await t.pumpAndSettle();
    expect(state.chats[sid]!.originPane, contains('e2e:'));

    await t.enterText(find.byKey(const ValueKey('composer')), '这是 App 一键接管后的消息，请只回复：E2E-TAKEOVER');
    await t.tap(find.byKey(const ValueKey('send')));
    await waitFor(t, () => state.chats[sid]!.items.any((i) => i.text.contains('E2E-TAKEOVER') && i.kind == ChatKind.assistant),
        what: 'reply');
    await waitFor(t, () => !state.chats[sid]!.promptRunning, what: 'prompt.done');

    // detach with hand-back (checked by default)
    await tapAndSettle(t, find.widgetWithText(TextButton, '断开'));
    await t.tap(find.byKey(const ValueKey('confirm-detach')));
    await waitFor(t, () => !state.chats.containsKey(sid), seconds: 30, what: 'detach');
    await waitFor(t, () => lock.existsSync(), seconds: 60, what: 'terminal resumed the session');
    final pane = await Process.run('tmux', ['-L', 'agentdock', 'display', '-p', '-t', 'e2e', '#{pane_current_command}']);
    expect('${pane.stdout}'.trim(), 'kiro-cli', reason: 'session handed back to the pane');

    // folder picker: navigate to spike/workdir, mkdir, rename, create a session there
    final stamp = DateTime.now().millisecondsSinceEpoch;
    ScaffoldMessenger.of(t.element(find.byType(Scaffold).first)).clearSnackBars();
    state.navIndex.value = 0;
    await t.pumpAndSettle();
    await tapAndSettle(t, find.byKey(const ValueKey('new-session')));
    for (final d in ['Project', 'AgentDock', 'spike', 'workdir']) {
      await tapAndSettle(t, find.byKey(ValueKey('dir-$d')));
    }
    await tapAndSettle(t, find.byKey(const ValueKey('mkdir')));
    await t.enterText(find.byKey(const ValueKey('folder-name')), 'e2e_new_$stamp');
    await tapAndSettle(t, find.byKey(const ValueKey('folder-name-ok')));
    expect(Directory('$workdir/e2e_new_$stamp').existsSync(), isTrue);
    await tapAndSettle(t, find.byKey(ValueKey('rename-e2e_new_$stamp')));
    await t.enterText(find.byKey(const ValueKey('folder-name')), 'e2e_dir_$stamp');
    await tapAndSettle(t, find.byKey(const ValueKey('folder-name-ok')));
    expect(Directory('$workdir/e2e_dir_$stamp').existsSync(), isTrue);
    expect(Directory('$workdir/e2e_new_$stamp').existsSync(), isFalse);
    await tapAndSettle(t, find.byKey(ValueKey('dir-e2e_dir_$stamp')));
    await t.tap(find.byKey(const ValueKey('create-here')));
    await waitFor(t, () => state.chats.values.any((c) => c.cwd == '$workdir/e2e_dir_$stamp'), seconds: 90, what: 'session.new');
    final created = state.chats.values.firstWhere((c) => c.cwd == '$workdir/e2e_dir_$stamp');
    await t.pumpAndSettle();
    await state.detach(created.sessionId);
    stdout.writeln('E2E_TAKEOVER_OK new=${created.sessionId} dir=e2e_dir_$stamp');
  });
}
