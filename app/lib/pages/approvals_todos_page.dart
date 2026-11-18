import 'package:flutter/material.dart';

import '../app_state.dart';
import '../daemon_client.dart';
import '../widgets/approval_card.dart';

class ApprovalsPage extends StatelessWidget {
  const ApprovalsPage({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('待审批')),
      body: ListenableBuilder(
        listenable: state,
        builder: (context, _) {
          final list = state.approvals.values.toList()
            ..sort((a, b) => ((a['createdAtMs'] ?? 0) as num).compareTo((b['createdAtMs'] ?? 0) as num));
          if (list.isEmpty) return const Center(child: Text('没有待审批的工具调用'));
          return ListView(
            padding: const EdgeInsets.all(12),
            children: [for (final a in list) ApprovalCard(state: state, request: a, showSession: true)],
          );
        },
      ),
    );
  }
}

class TodosPage extends StatelessWidget {
  const TodosPage({super.key, required this.state});
  final AppState state;

  Future<void> _run(BuildContext context, Future<void> Function() f) async {
    try {
      await f();
    } on DaemonError catch (e) {
      if (context.mounted) ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(e.message)));
    }
  }

  Future<void> _add(BuildContext context) async {
    final title = TextEditingController();
    final due = TextEditingController();
    String? priority;
    final ok = await showDialog<bool>(
      context: context,
      builder: (c) => StatefulBuilder(
        builder: (c, set) => AlertDialog(
          title: const Text('新增待办'),
          content: Column(mainAxisSize: MainAxisSize.min, children: [
            TextField(key: const ValueKey('todo-title'), controller: title, autofocus: true, decoration: const InputDecoration(labelText: '内容')),
            DropdownButtonFormField<String?>(
              initialValue: priority,
              decoration: const InputDecoration(labelText: '优先级'),
              items: const [
                DropdownMenuItem(value: null, child: Text('无')),
                DropdownMenuItem(value: 'high', child: Text('高')),
                DropdownMenuItem(value: 'medium', child: Text('中')),
                DropdownMenuItem(value: 'low', child: Text('低')),
              ],
              onChanged: (v) => set(() => priority = v),
            ),
            TextField(controller: due, decoration: const InputDecoration(labelText: '截止日期 YYYY-MM-DD（可空）')),
          ]),
          actions: [
            TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('取消')),
            FilledButton(key: const ValueKey('todo-save'), onPressed: () => Navigator.pop(c, true), child: const Text('添加')),
          ],
        ),
      ),
    );
    if (ok == true && context.mounted) {
      await _run(context, () => state.addTodo(title.text,
          priority: priority, dueDate: due.text.trim().isEmpty ? null : due.text.trim()));
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('待办'),
        actions: [
          Padding(
            padding: const EdgeInsets.only(right: 12),
            child: ListenableBuilder(
              listenable: state,
              builder: (context, _) => SegmentedButton<String>(
                segments: const [
                  ButtonSegment(value: 'open', label: Text('待办')),
                  ButtonSegment(value: 'done', label: Text('已完成')),
                  ButtonSegment(value: 'all', label: Text('全部')),
                ],
                selected: {state.todoFilter},
                onSelectionChanged: (s) => _run(context, () => state.setTodoFilter(s.first)),
              ),
            ),
          ),
        ],
      ),
      floatingActionButton: FloatingActionButton(
        key: const ValueKey('todo-add'),
        heroTag: null, // several pages stay mounted in an IndexedStack
        tooltip: '新增待办',
        onPressed: () => _add(context),
        child: const Icon(Icons.add),
      ),
      body: ListenableBuilder(
        listenable: state,
        builder: (context, _) {
          if (state.todos.isEmpty) return const Center(child: Text('暂无待办'));
          return ListView(
            padding: const EdgeInsets.only(bottom: 88),
            children: [for (final t in state.todos) _tile(context, t)],
          );
        },
      ),
    );
  }

  Widget _tile(BuildContext context, Json t) {
    final id = t['id'] as int;
    final done = t['status'] == 'done';
    final sid = t['sessionId'] as String?;
    final meta = <String>[
      if (t['priority'] != null) '优先级 ${const {'high': '高', 'medium': '中', 'low': '低'}[t['priority']] ?? t['priority']}',
      if (t['dueDate'] != null) '截止 ${t['dueDate']}',
      t['source'] == 'auto' ? '来自 ${t['agent'] ?? 'AI'} 会话 ${sid?.substring(0, 8) ?? ''}' : '手动添加',
    ];
    return ListTile(
      key: ValueKey('todo-$id'),
      leading: Checkbox(value: done, onChanged: (v) => _run(context, () => state.setTodoDone(id, v == true))),
      title: Text('${t['title']}', style: done ? const TextStyle(decoration: TextDecoration.lineThrough) : null),
      subtitle: Text(meta.join(' · ')),
      trailing: Wrap(children: [
        if (sid != null && state.chats.containsKey(sid))
          IconButton(
            tooltip: '打开来源会话',
            icon: const Icon(Icons.forum_outlined),
            onPressed: () {
              state.currentChat.value = sid;
              state.navIndex.value = 1;
            },
          ),
        IconButton(tooltip: '删除', icon: const Icon(Icons.delete_outline), onPressed: () => _run(context, () => state.deleteTodo(id))),
      ]),
    );
  }
}
