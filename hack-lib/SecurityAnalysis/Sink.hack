namespace Hakana\SecurityAnalysis;

/**
 * Used to denote a sink in taint/security analysis. It can have one or more taint types.
 */
final class Sink implements \HH\ParameterAttribute, \HH\InstancePropertyAttribute {
	public function __construct(string ...$types) {}
}
