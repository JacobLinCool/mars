class MarsClientError(Exception):
    def __init__(self, message: str, *, exit_code: int | None = None) -> None:
        super().__init__(message)
        self.exit_code = exit_code


class ProtocolVersionError(MarsClientError):
    pass


class DaemonError(MarsClientError):
    pass
