unit Runner;

interface

uses Worker;

type
  TRunner = class(TWorker)
  public
    procedure Execute;
  end;

implementation

procedure TRunner.Execute;
begin
  Process;
end;

end.
