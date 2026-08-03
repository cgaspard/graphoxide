unit Worker;

interface

type
  TWorker = class
  public
    procedure Process; virtual;
  end;

implementation

procedure TWorker.Process;
begin
end;

end.
