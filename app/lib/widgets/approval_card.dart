import 'package:flutter/material.dart';

import '../app_state.dart';
import '../daemon_client.dart';

const _optionLabels = {
  'allow_once': '允许一次',
  'allow_always': '总是允许',
  'reject_once': '拒绝',
};

String optionLabel(String id) => _optionLabels[id] ?? id;

/// Shows exactly what will run (rawInput) and the agent-offered options.
class ApprovalCard extends StatefulWidget {
  const ApprovalCard({
    super.key,
    required this.state,
    required this.request,
    this.resolution,
    this.showSession = false,
  });

  final AppState state;
  final Json request;
  final String? resolution;
  final bool showSession;

  @override
  State<ApprovalCard> createState() => _ApprovalCardState();
}

class _ApprovalCardState extends State<ApprovalCard> {
  bool _busy = false;

  Future<void> _reply(String optionId) async {
    if (optionId == 'allow_always') {
      final ok = await showDialog<bool>(
        context: context,
        builder: (c) => AlertDialog(
          title: const Text('确认"总是允许"？'),
          content: const Text('Kiro 可能会把这条信任规则保存下来，之后同类调用不再询问，也会影响终端里的会话。'),
          actions: [
            TextButton(onPressed: () => Navigator.pop(c, false), child: const Text('取消')),
            FilledButton(onPressed: () => Navigator.pop(c, true), child: const Text('总是允许')),
          ],
        ),
      );
      if (ok != true) return;
    }
    setState(() => _busy = true);
    try {
      await widget.state.replyApproval('${widget.request['approvalId']}', optionId);
    } on DaemonError catch (e) {
      if (mounted) {
        ScaffoldMessenger.of(context).showSnackBar(SnackBar(content: Text(e.message)));
      }
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    final r = widget.request;
    final scheme = Theme.of(context).colorScheme;
    final pending = widget.resolution == null;
    final unknown = r['inputUnknown'] == true;
    final options = List<Map<String, dynamic>>.from(r['options'] ?? const []);
    return Card(
      key: ValueKey('approval-${r['approvalId']}'),
      color: pending ? scheme.tertiaryContainer : scheme.surfaceContainerHighest,
      margin: const EdgeInsets.symmetric(vertical: 6),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Row(children: [
              Icon(pending ? Icons.gpp_maybe : Icons.verified_user, size: 20),
              const SizedBox(width: 8),
              Expanded(
                child: Text(
                  '审批请求 ${r['approvalId']} · ${r['toolName'] ?? '?'}（${r['kind'] ?? '?'}）',
                  style: const TextStyle(fontWeight: FontWeight.bold),
                ),
              ),
            ]),
            if (widget.showSession)
              Text('会话 ${r['sessionId']}', style: Theme.of(context).textTheme.bodySmall),
            const SizedBox(height: 6),
            Text('${r['title'] ?? ''}'),
            const SizedBox(height: 6),
            if (unknown)
              Text('⚠ 未能获取完整参数，请谨慎批准', style: TextStyle(color: scheme.error, fontWeight: FontWeight.bold))
            else
              Container(
                width: double.infinity,
                padding: const EdgeInsets.all(8),
                decoration: BoxDecoration(
                  color: scheme.surface,
                  borderRadius: BorderRadius.circular(6),
                ),
                child: SelectableText(
                  prettyJson(r['rawInput']),
                  style: const TextStyle(fontFamily: 'monospace', fontSize: 13),
                ),
              ),
            const SizedBox(height: 8),
            if (pending)
              Wrap(
                spacing: 8,
                children: [
                  for (final o in options)
                    _button('${o['optionId']}', '${o['kind'] ?? ''}'),
                ],
              )
            else
              Text('已处理：${optionLabel(widget.resolution!)}',
                  style: const TextStyle(fontWeight: FontWeight.bold)),
          ],
        ),
      ),
    );
  }

  Widget _button(String optionId, String kind) {
    final label = Text(optionLabel(optionId));
    final onPressed = _busy ? null : () => _reply(optionId);
    final key = ValueKey('opt-$optionId-${widget.request['approvalId']}');
    if (kind.startsWith('reject')) {
      return OutlinedButton(key: key, onPressed: onPressed, child: label);
    }
    if (kind == 'allow_once') {
      return FilledButton(key: key, onPressed: onPressed, child: label);
    }
    return TextButton(key: key, onPressed: onPressed, child: label);
  }
}
