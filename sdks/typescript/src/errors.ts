export class MarsClientError extends Error {
  public constructor(message: string, public readonly exitCode?: number) {
    super(message);
    this.name = new.target.name;
  }
}

export class ProtocolVersionError extends MarsClientError {}
export class DaemonError extends MarsClientError {}
