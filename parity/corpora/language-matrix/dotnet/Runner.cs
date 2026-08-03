namespace Matrix.Runtime;

public sealed class Runner : Service
{
    private readonly IWorker _worker;

    public Runner(IWorker worker)
    {
        _worker = worker;
    }

    public string Execute(string value) => _worker.Process(Process(value));
}
