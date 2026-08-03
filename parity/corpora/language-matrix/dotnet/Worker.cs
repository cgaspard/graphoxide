namespace Matrix.Runtime;

public interface IWorker
{
    string Process(string value);
}

public class Service : IWorker
{
    public virtual string Process(string value) => value.Trim();
}
