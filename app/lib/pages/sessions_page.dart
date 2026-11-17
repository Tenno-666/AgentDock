import 'package:flutter/material.dart';

import '../app_state.dart';
import '../daemon_client.dart';

class SessionsPage extends StatelessWidget {
  const SessionsPage({super.key, required this.state});
  final AppState state;

  Future<void> _run(BuildContext context, Future<void> Function() f) async {
    try {
      await f();
    } on DaemonError catch (e) {
      if (context.mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(e.message), duration: const Duration(seconds: 6)));
      }
    }
  }

  Future<void> _newSession(BuildContext context) async {
    final ctl = TextEditingController(text: '/home/mi/Project/AgentDock/spike/workdir');
    final cwd = await showDialog<String>(
      context: context,
      builder: (c) => AlertDialog(
        title: const Text('新建会话'),
        content: TextField(
          controller: ctl,
          decoration: const InputDecoration(labelText: '工作目录（HOME 下的绝对路径）'),
          autofocus: true,
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(c), child: const Text('取消')),
          FilledButton(onPressed: () => Navigator.pop(c, ctl.text.trim()), child: const Text('创建')),
        ],
      ),
    );
    if (cwd != null && cwd.isNotEmpty && context.mounted) {
      await _run(context, () => state.newSession(cwd));
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('会话'),
        actions: [
          IconButton(tooltip: '刷新', icon: const Icon(Icons.refresh), onPressed: () => _run(context, state.refreshSessions)),
        ],
      ),
      floatingActionButton: FloatingActionButton.extended(
        key: const ValueKey('new-session'),
        onPressed: () => _newSession(context),
        icon: const Icon(Icons.add),
        label: const Text('新建会话'),
      ),
      body: ListenableBuilder(
        listenable: state,
        builder: (context, _) {
          final attachedIds = state.chats.entries.where((e) => !e.value.closed).map((e) => e.key).toSet();
          final active = state.activeSessions;
          final daemonOnly = attachedIds.where((id) => !active.any((s) => s['sessionId'] == id)).toList();
          return ListView(
            padding: const EdgeInsets.only(bottom: 88),
            children: [
              _header(context, '本机活跃会话（${active.length}）'),
              if (active.isEmpty) const ListTile(title: Text('没有发现正在运行的 Kiro 会话')),
              for (final s in active) _sessionTile(context, s, attachedIds.contains(s['sessionId'])),
              if (daemonOnly.isNotEmpty) _header(context, '仅在 AgentDock 中（${daemonOnly.length}）'),
              for (final id in daemonOnly)
                ListTile(
                  leading: const Icon(Icons.link),
                  title: Text(id),
                  subtitle: Text(state.chats[id]?.cwd ?? ''),
                  onTap: () {
                    state.currentChat.value = id;
                    state.navIndex.value = 1;
                  },
                ),
            ],
          );
        },
      ),
    );
  }

  Widget _header(BuildContext context, String text) => Padding(
    padding: const EdgeInsets.fromLTRB(16, 16, 16, 4),
    child: Text(text, style: Theme.of(context).textTheme.titleSmall),
  );

  Widget _sessionTile(BuildContext context, Json s, bool attached) {
    final sid = '${s['sessionId']}';
    final pane = s['tmuxPane'] as String?;
    final status = attached ? '已接管' : (pane != null ? '终端中 · $pane' : '其他进程中');
    return ListTile(
      key: ValueKey('session-$sid'),
      leading: Icon(attached ? Icons.link : Icons.terminal),
      title: Text('${s['title'] ?? sid}', maxLines: 1, overflow: TextOverflow.ellipsis),
      subtitle: Text('$status\n${s['cwd'] ?? '-'} · ${sid.substring(0, 8)}'),
      isThreeLine: true,
      trailing: Wrap(spacing: 4, children: [
        TextButton(
          onPressed: () => Navigator.of(context).push(MaterialPageRoute(builder: (_) => HistoryPage(state: state, sessionId: sid, title: '${s['title'] ?? sid}'))),
          child: const Text('查看'),
        ),
        if (attached)
          FilledButton.tonal(
            onPressed: () {
              state.currentChat.value = sid;
              state.navIndex.value = 1;
            },
            child: const Text('打开'),
          )
        else
          FilledButton(
            key: ValueKey('attach-$sid'),
            onPressed: () => _run(context, () => state.attach(sid)),
            child: const Text('接管'),
          ),
      ]),
    );
  }
}

/// Read-only history from the session file (does not disturb the terminal).
class HistoryPage extends StatelessWidget {
  const HistoryPage({super.key, required this.state, required this.sessionId, required this.title});
  final AppState state;
  final String sessionId;
  final String title;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: Text(title, overflow: TextOverflow.ellipsis)),
      body: FutureBuilder<Json?>(
        future: state.viewHistory(sessionId),
        builder: (context, snap) {
          if (snap.hasError) return Center(child: Text('读取失败：${snap.error}'));
          if (!snap.hasData) return const Center(child: CircularProgressIndicator());
          final entries = List<Json>.from(snap.data!['view']?['entries'] ?? const []);
          return ListView.builder(
            padding: const EdgeInsets.all(12),
            itemCount: entries.length,
            itemBuilder: (_, i) {
              final e = entries[i];
              switch (e['role']) {
                case 'user':
                  return _bubble(context, '${e['text']}', true);
                case 'assistant':
                  final tools = List<Json>.from(e['toolUses'] ?? const []);
                  return Column(crossAxisAlignment: CrossAxisAlignment.start, children: [
                    if ('${e['text']}'.isNotEmpty) _bubble(context, '${e['text']}', false),
                    for (final t in tools)
                      Text('🔧 ${t['name']} ${t['input']}', maxLines: 3, overflow: TextOverflow.ellipsis,
                          style: Theme.of(context).textTheme.bodySmall),
                  ]);
                case 'compaction':
                  return const Text('— 上下文压缩 —', textAlign: TextAlign.center);
                default:
                  return const SizedBox.shrink();
              }
            },
          );
        },
      ),
    );
  }
}

Widget _bubble(BuildContext context, String text, bool user) {
  final scheme = Theme.of(context).colorScheme;
  return Align(
    alignment: user ? Alignment.centerRight : Alignment.centerLeft,
    child: Container(
      constraints: const BoxConstraints(maxWidth: 720),
      margin: const EdgeInsets.symmetric(vertical: 4),
      padding: const EdgeInsets.all(10),
      decoration: BoxDecoration(
        color: user ? scheme.primaryContainer : scheme.surfaceContainerHigh,
        borderRadius: BorderRadius.circular(10),
      ),
      child: SelectableText(text),
    ),
  );
}
