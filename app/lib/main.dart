import 'package:flutter/material.dart';

import 'app_state.dart';
import 'daemon_client.dart';
import 'pages/approvals_todos_page.dart';
import 'pages/chat_page.dart';
import 'pages/sessions_page.dart';

const daemonUrl = String.fromEnvironment('AGENTDOCK_URL', defaultValue: 'ws://127.0.0.1:7820/ws');

void main() {
  final state = AppState(DaemonClient(Uri.parse(daemonUrl)));
  state.client.connect();
  runApp(AgentDockApp(state: state));
}

class AgentDockApp extends StatelessWidget {
  const AgentDockApp({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'AgentDock',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: HomeShell(state: state),
    );
  }
}

class HomeShell extends StatelessWidget {
  const HomeShell({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([state, state.navIndex]),
      builder: (context, _) {
        final index = state.navIndex.value;
        final pages = [
          SessionsPage(state: state),
          ChatPage(state: state),
          ApprovalsPage(state: state),
          TodosPage(state: state),
        ];
        Widget badge(IconData icon, int n) => Badge(isLabelVisible: n > 0, label: Text('$n'), child: Icon(icon));
        final dests = [
          (const Icon(Icons.dns_outlined), '会话'),
          (badge(Icons.forum_outlined, state.chats.values.where((c) => !c.closed).length), '对话'),
          (badge(Icons.gpp_maybe_outlined, state.approvals.length), '审批'),
          (badge(Icons.checklist, state.openTodoCount), '待办'),
        ];
        final body = Column(children: [
          if (state.conn != ConnState.connected) _ConnBanner(state: state),
          Expanded(child: IndexedStack(index: index, children: pages)),
        ]);
        final wide = MediaQuery.sizeOf(context).width >= 720;
        void go(int i) => state.navIndex.value = i;
        if (wide) {
          return Scaffold(
            body: Row(children: [
              NavigationRail(
                selectedIndex: index,
                onDestinationSelected: go,
                labelType: NavigationRailLabelType.all,
                destinations: [for (final d in dests) NavigationRailDestination(icon: d.$1, label: Text(d.$2))],
              ),
              const VerticalDivider(width: 1),
              Expanded(child: body),
            ]),
          );
        }
        return Scaffold(
          body: body,
          bottomNavigationBar: NavigationBar(
            selectedIndex: index,
            onDestinationSelected: go,
            destinations: [for (final d in dests) NavigationDestination(icon: d.$1, label: d.$2)],
          ),
        );
      },
    );
  }
}

class _ConnBanner extends StatelessWidget {
  const _ConnBanner({required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    final connecting = state.conn == ConnState.connecting;
    return MaterialBanner(
      content: Text(connecting ? '正在连接 daemon（$daemonUrl）…' : '未连接 daemon（$daemonUrl），正在自动重连。请确认 agentdock-daemon 已启动。'),
      leading: Icon(connecting ? Icons.sync : Icons.cloud_off),
      actions: [TextButton(onPressed: state.client.connect, child: const Text('立即重连'))],
    );
  }
}
