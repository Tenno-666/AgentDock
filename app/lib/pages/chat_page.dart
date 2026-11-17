import 'package:flutter/material.dart';
import 'package:flutter/services.dart';

import '../app_state.dart';
import '../daemon_client.dart';
import '../widgets/approval_card.dart';

class ChatPage extends StatelessWidget {
  const ChatPage({super.key, required this.state});
  final AppState state;

  @override
  Widget build(BuildContext context) {
    return ListenableBuilder(
      listenable: Listenable.merge([state, state.currentChat]),
      builder: (context, _) {
        final ids = state.chats.keys.toList();
        if (ids.isEmpty) {
          return Scaffold(
            appBar: AppBar(title: const Text('对话')),
            body: const Center(child: Text('还没有接管的会话。\n在"会话"页点"接管"或"新建会话"。', textAlign: TextAlign.center)),
          );
        }
        var current = state.currentChat.value;
        if (current == null || !state.chats.containsKey(current)) current = ids.first;
        final idx = ids.indexOf(current);
        return DefaultTabController(
          key: ValueKey('tabs-${ids.join(',')}'),
          length: ids.length,
          initialIndex: idx,
          child: Builder(builder: (context) {
            final ctl = DefaultTabController.of(context);
            ctl.addListener(() {
              if (!ctl.indexIsChanging) state.currentChat.value = ids[ctl.index];
            });
            return Scaffold(
              appBar: AppBar(
                title: const Text('对话'),
                bottom: TabBar(
                  isScrollable: true,
                  tabs: [
                    for (final id in ids)
                      Tab(
                        child: Row(mainAxisSize: MainAxisSize.min, children: [
                          Text(id.substring(0, 8)),
                          if (state.approvals.values.any((a) => a['sessionId'] == id))
                            const Padding(padding: EdgeInsets.only(left: 4), child: Icon(Icons.circle, size: 8, color: Colors.red))
                          else if (state.chats[id]!.promptRunning)
                            const Padding(padding: EdgeInsets.only(left: 6), child: SizedBox(width: 10, height: 10, child: CircularProgressIndicator(strokeWidth: 2))),
                        ]),
                      ),
                  ],
                ),
              ),
              body: TabBarView(
                children: [for (final id in ids) _ChatView(key: ValueKey('chat-$id'), state: state, chat: state.chats[id]!)],
              ),
            );
          }),
        );
      },
    );
  }
}

class _ChatView extends StatefulWidget {
  const _ChatView({super.key, required this.state, required this.chat});
  final AppState state;
  final ChatSession chat;

  @override
  State<_ChatView> createState() => _ChatViewState();
}

class _ChatViewState extends State<_ChatView> {
  final _input = TextEditingController();
  final _scroll = ScrollController();
  final _focus = FocusNode();
  bool _showThoughts = false;

  @override
  void dispose() {
    _input.dispose();
    _scroll.dispose();
    _focus.dispose();
    super.dispose();
  }

  void _snack(String msg) {
    if (mounted) ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(msg)));
  }

  Future<void> _send() async {
    final text = _input.text.trim();
    if (text.isEmpty) return;
    try {
      await widget.state.sendPrompt(widget.chat.sessionId, text);
      _input.clear();
    } on DaemonError catch (e) {
      _snack(e.message);
    }
  }

  Future<void> _detach() async {
    final ok = await showDialog<bool>(
      context: context,
      builder: (c) => AlertDialog(
        title: const Text('断开会话？'),
        content: SelectableText(
          '断开后可在终端续接：\nkiro-cli --resume-id ${widget.chat.sessionId}\n\n'
          '如果终端提示 "Failed to resume session"，说明续接超时并已静默新开会话，请 /quit 后重试。',
        ),
        actions: [
          TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('取消')),
          FilledButton(onPressed: () => Navigator.pop(c, true), child: const Text('断开')),
        ],
      ),
    );
    if (ok != true) return;
    try {
      await widget.state.detach(widget.chat.sessionId);
    } on DaemonError catch (e) {
      _snack(e.message);
    }
  }

  void _autoScroll() {
    WidgetsBinding.instance.addPostFrameCallback((_) {
      if (_scroll.hasClients && _scroll.position.maxScrollExtent - _scroll.offset < 400) {
        _scroll.jumpTo(_scroll.position.maxScrollExtent);
      }
    });
  }

  @override
  Widget build(BuildContext context) {
    final chat = widget.chat;
    final items = chat.items.where((i) => _showThoughts || i.kind != ChatKind.thought).toList();
    _autoScroll();
    return Column(children: [
      Material(
        elevation: 1,
        child: Padding(
          padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 4),
          child: Row(children: [
            Expanded(child: SelectableText('${chat.sessionId}  ·  ${chat.cwd}', maxLines: 1, style: Theme.of(context).textTheme.bodySmall)),
            FilterChip(label: const Text('思考过程'), selected: _showThoughts, onSelected: (v) => setState(() => _showThoughts = v)),
            const SizedBox(width: 8),
            if (!chat.closed) TextButton.icon(onPressed: _detach, icon: const Icon(Icons.link_off), label: const Text('断开')),
          ]),
        ),
      ),
      Expanded(
        child: ListView.builder(
          controller: _scroll,
          padding: const EdgeInsets.all(12),
          itemCount: items.length,
          itemBuilder: (_, i) => _item(context, items[i]),
        ),
      ),
      if (!chat.closed) _composer(context, chat),
    ]);
  }

  Widget _composer(BuildContext context, ChatSession chat) {
    return SafeArea(
      top: false,
      child: Padding(
        padding: const EdgeInsets.fromLTRB(12, 4, 12, 12),
        child: Row(crossAxisAlignment: CrossAxisAlignment.end, children: [
          Expanded(
            child: CallbackShortcuts(
              bindings: {
                const SingleActivator(LogicalKeyboardKey.enter, control: true): _send,
              },
              child: TextField(
                key: const ValueKey('composer'),
                controller: _input,
                focusNode: _focus,
                minLines: 1,
                maxLines: 8,
                keyboardType: TextInputType.multiline,
                decoration: const InputDecoration(hintText: '输入消息（Ctrl+Enter 发送，Enter 换行）', border: OutlineInputBorder()),
              ),
            ),
          ),
          const SizedBox(width: 8),
          if (chat.promptRunning)
            IconButton.filledTonal(
              tooltip: '取消本轮',
              onPressed: () => widget.state.cancelPrompt(chat.sessionId).catchError((Object e) => _snack('$e')),
              icon: const Icon(Icons.stop),
            )
          else
            IconButton.filled(key: const ValueKey('send'), tooltip: '发送', onPressed: _send, icon: const Icon(Icons.send)),
        ]),
      ),
    );
  }

  Widget _item(BuildContext context, ChatItem it) {
    final scheme = Theme.of(context).colorScheme;
    switch (it.kind) {
      case ChatKind.user:
      case ChatKind.assistant:
        final user = it.kind == ChatKind.user;
        return Align(
          alignment: user ? Alignment.centerRight : Alignment.centerLeft,
          child: Container(
            constraints: const BoxConstraints(maxWidth: 760),
            margin: const EdgeInsets.symmetric(vertical: 4),
            padding: const EdgeInsets.all(10),
            decoration: BoxDecoration(
              color: user ? scheme.primaryContainer : scheme.surfaceContainerHigh,
              borderRadius: BorderRadius.circular(10),
              border: it.replay ? Border.all(color: scheme.outlineVariant) : null,
            ),
            child: SelectableText(it.text),
          ),
        );
      case ChatKind.thought:
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 2),
          child: Text(it.text, style: TextStyle(color: scheme.outline, fontStyle: FontStyle.italic, fontSize: 12)),
        );
      case ChatKind.tool:
        final d = it.data ?? {};
        final status = '${d['status'] ?? d['phase'] ?? ''}';
        final icon = switch (status) {
          'completed' => Icons.check_circle,
          'failed' => Icons.cancel,
          _ => Icons.build_circle,
        };
        return Card(
          margin: const EdgeInsets.symmetric(vertical: 3),
          child: ExpansionTile(
            dense: true,
            leading: Icon(icon, size: 18),
            title: Text('${d['title'] ?? '工具调用'}', maxLines: 1, overflow: TextOverflow.ellipsis),
            subtitle: Text(status),
            children: [
              Padding(
                padding: const EdgeInsets.fromLTRB(12, 0, 12, 8),
                child: SelectableText(
                  '${prettyJson(d['rawInput'])}${'${d['output'] ?? ''}'.isEmpty ? '' : '\n→ ${d['output']}'}',
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
                ),
              ),
            ],
          ),
        );
      case ChatKind.approval:
        return ApprovalCard(state: widget.state, request: it.data!, resolution: it.resolution);
      case ChatKind.notice:
        return Padding(
          padding: const EdgeInsets.symmetric(vertical: 6),
          child: Center(child: Text(it.text, style: TextStyle(color: scheme.outline))),
        );
    }
  }
}
