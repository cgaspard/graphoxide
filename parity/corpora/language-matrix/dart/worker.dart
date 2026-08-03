abstract class Worker {
  String process(String value);
}

class Service implements Worker {
  @override
  String process(String value) => value.trim();
}
