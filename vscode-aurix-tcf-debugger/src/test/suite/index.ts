import * as path from 'path';
import * as fs from 'fs';
import * as Mocha from 'mocha';

export function run(): Promise<void> {
	const useReport = process.env.AURIX_TEST_REPORT === '1' || process.env.AURIX_TEST_REPORT === 'true';
	const reporter = process.env.MOCHA_REPORTER || (useReport ? 'mocha-junit-reporter' : 'spec');
	const reportFile = process.env.MOCHA_FILE || (useReport ? path.resolve('test-results', 'junit.xml') : '');
	const reporterOptions = reportFile ? { mochaFile: reportFile } : undefined;

	if (reportFile) {
		const dir = path.dirname(reportFile);
		fs.mkdirSync(dir, { recursive: true });
	}

	const mocha = new Mocha({ ui: 'tdd', timeout: 120000, reporter, reporterOptions });
	const testsRoot = path.resolve(__dirname);
	mocha.addFile(path.join(testsRoot, 'liveUpdate.test'));
	mocha.addFile(path.join(testsRoot, 'liveWatchEvaluator.test'));
	mocha.addFile(path.join(testsRoot, 'svdCatalog.test'));
	mocha.addFile(path.join(testsRoot, 'svdMerge.test'));
	mocha.addFile(path.join(testsRoot, 'sourceFileMap.test'));
	mocha.addFile(path.join(testsRoot, 'breakpoint.stopat.test'));
	mocha.addFile(path.join(testsRoot, 'debug.stress.test'));
	return new Promise((resolve, reject) => {
		mocha.run((failures) => (failures ? reject(new Error(`${failures} failures`)) : resolve()));
	});
}
