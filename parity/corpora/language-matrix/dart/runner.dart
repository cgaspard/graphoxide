import 'worker.dart';
import 'package:flutter/material.dart';

class Runner extends Service {
  String execute(String value) => process(value);
}

class WorkerPanel extends StatefulWidget {
  const WorkerPanel({super.key});

  @override
  State<WorkerPanel> createState() => _WorkerPanelState();
}

class _WorkerPanelState extends State<WorkerPanel> {
  final Service service = Service();

  @override
  Widget build(BuildContext context) {
    return StreamBuilder<String>(
      stream: const Stream<String>.empty(),
      builder: (context, snapshot) => Text(service.process(snapshot.data ?? '')),
    );
  }
}
