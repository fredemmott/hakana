abstract class Validator<+T> {
    <<__LateInit>> private T $input;

    <<\Hakana\SecurityAnalysis\Source('UriRequestHeader')>>
    public function getInput(): T {
        return $this->input;
    }
}

abstract class Result<+T, +TErr> {
	abstract public function get(): T;
}

final class ResultSuccess<+T> extends Result<T, nothing> {
	public function __construct(private T $t) {}
	public function get(): T {
		return $this->t;
	}
}

final class ResultError extends Result<nothing, string> {
	public function __construct(private string $message) {}
    public function get(): T {
		throw new \Exception('bad');
	}
}

abstract class InputHandler<TArgs> {
    public Validator<TArgs> $validator;

    public function __construct(public Validator<TArgs> $validator) {}

    public function getValidatedInput() {
        $input = $this->validator->getInput();

        $this->handleResult($input);
    }

    async public function handleResult(TArgs $args): Awaitable<Result<mixed>> {
        $a = await $this->getResult($args);
        if ($a is ResultSuccess<_>) {
            echo $a->get();
        }
        return $a;
    }

    abstract public function getResult(TArgs $args): Awaitable<mixed>;
}

type my_args_t = shape('a' => string);

abstract class BHandler extends InputHandler<my_args_t> {}

class MyHandler extends BHandler {
    async public function getResult(my_args_t $args): Awaitable<Result<string>> {
        return new ResultSuccess($args['a']);
    }
}