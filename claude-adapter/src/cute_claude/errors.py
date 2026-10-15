from typing import Any


class RpcError(Exception):
    def __init__(self, code: int, message: str, data: Any = None):
        super().__init__(message)
        self.code, self.message, self.data = code, message, data

    def wire(self) -> dict:
        result = {"code": self.code, "message": self.message}
        if self.data is not None:
            result["data"] = self.data
        return result


def require(condition: bool, message: str, code: int = -32602) -> None:
    if not condition:
        raise RpcError(code, message)


def text(value: Any, name: str, *, limit: int = 65536) -> str:
    require(isinstance(value, str) and bool(value.strip()), f"{name} must be a nonempty string")
    require(len(value.encode("utf-8")) <= limit, f"{name} exceeds {limit} bytes")
    return value
