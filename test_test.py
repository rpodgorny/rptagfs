import pytest
import tempfile
import shutil
import subprocess
import os
import time


def call(cmd):
	return subprocess.check_output(cmd, shell=True)


def write_to_file(fn, data):
	with open(fn, 'w') as f:
		f.write(data)


def read_from_file(fn):
	with open(fn, 'r') as f:
		return f.read()


def get_dir_contents(directory):
	return [dirs for (_, dirs, _) in os.walk(directory)]


@pytest.fixture
def tmpdir():
	t = tempfile.mkdtemp()
	yield t
	shutil.rmtree(t)


# TODO: find a way to deduplicate
@pytest.fixture
def tmpdir2():
	t = tempfile.mkdtemp()
	yield t
	shutil.rmtree(t)


@pytest.fixture
def mountdir(tmpdir, tmpdir2):
	cmd = './run_test.sh -d -oroot=%s %s >/tmp/logggg 2>/tmp/errlogggg' % (tmpdir2, tmpdir)
	#call(cmd)
	p = subprocess.Popen(cmd, shell=True)
	time.sleep(2)  # TODO: ugly hack
	yield tmpdir
	call('fusermount -u %s' % tmpdir)
	p.communicate()
	p.terminate()
	p.wait()


@pytest.fixture
def playdir(tmpdir):
	cwd_ = os.getcwd()
	os.chdir(tmpdir)
	os.mkdir('src')
	write_to_file('src/file1.txt', 'content1')
	os.makedirs('src/tag1/tag2/tag3/empty_tag')
	write_to_file('src/tag1/tag2/tag3/file2.txt', 'content2')
	os.makedirs('src/tag_dup/tag_dup/tag_dup/tag1/tag_dup')
	write_to_file('src/tag_dup/tag_dup/tag_dup/tag1/tag_dup/file3.txt', 'content3')
	os.mkdir('mnt')
	os.chdir(cwd_)
	yield tmpdir


@pytest.fixture
def mntdir(playdir):
	cmd = './run_test.sh -d -oroot=%s/src %s/mnt >stdout 2>stderr' % (playdir, playdir)
	#call(cmd)
	p = subprocess.Popen(cmd, shell=True)
	time.sleep(2)  # TODO: ugly hack
	cwd_ = os.getcwd()
	os.chdir(playdir)
	yield playdir
	os.chdir(cwd_)
	call('fusermount -u %s/mnt' % playdir)
	p.communicate()
	p.terminate()
	p.wait()
	print('stdout:')
	print(read_from_file('stdout'))
	print('stderr:')
	print(read_from_file('stderr'))


def test_playdir(mntdir):
	os.stat('mnt/file1.txt')
	os.chmod('mnt/file1.txt', 0o777)

	assert read_from_file('mnt/file1.txt') == 'content1'
	assert not os.path.exists('mnt/tag1/file1.txt')
	assert read_from_file('mnt/tag1/file2.txt') == 'content2'
	assert read_from_file('mnt/tag2/file2.txt') == 'content2'
	assert read_from_file('mnt/tag3/file2.txt') == 'content2'
	assert read_from_file('mnt/tag1/tag3/file2.txt') == 'content2'
	assert read_from_file('mnt/tag3/tag1/file2.txt') == 'content2'
	#assert os.listdir('mnt/empty_tag') == []
	assert 'empty_tag' in os.listdir('mnt')
	os.rmdir('mnt/tag3/tag2/tag1/empty_tag')
	assert 'empty_tag' not in os.listdir('mnt')

	os.mkdir('mnt/tag1/new_tag')


def test_rename_tag_dup(mntdir):
	assert 'tag_dup' in os.listdir('mnt')
	assert 'tag_dup2' not in os.listdir('mnt')
	os.rename('mnt/tag_dup', 'mnt/tag_dup2')
	assert 'tag_dup' not in os.listdir('mnt')
	assert 'tag_dup2' in os.listdir('mnt')


def test_rename_tag(mntdir):
	assert 'tag1' in os.listdir('mnt')
	assert 'tag11' not in os.listdir('mnt')
	os.rename('mnt/tag1', 'mnt/tag11')
	#call('mv -v mnt/tag1 mnt/tag11')
	assert 'tag1' not in os.listdir('mnt')
	assert 'tag11' in os.listdir('mnt')


def test_rename_file(mntdir):
	assert 'file2.txt' in os.listdir('mnt/tag1')
	assert 'file22.txt' not in os.listdir('mnt/tag1')
	os.rename('mnt/tag1/file2.txt', 'mnt/tag1/file22.txt')
	assert 'file2.txt' not in os.listdir('mnt/tag1')
	assert 'file22.txt' in os.listdir('mnt/tag1')


def test_remove_file(mntdir):
	assert 'file2.txt' in os.listdir('mnt/tag1')
	os.remove('mnt/tag1/file2.txt')
	assert 'file2.txt' not in os.listdir('mnt/tag1')
	assert 'file2.txt' not in os.listdir('mnt')


def test_create_file(mountdir):
	#assert get_dir_contents(mountdir) == []
	#print(call('echo "ahoj" >%s/pokus.txt' % mountdir))
	#print(call('ls -l %s' % mountdir))

	assert 'file.txt' not in os.listdir(mountdir)
	write_to_file('%s/file.txt' % mountdir, 'ahoj')
	assert read_from_file('%s/file.txt' % mountdir) == 'ahoj'
	assert 'file.txt' in os.listdir(mountdir)


def test_add_tags(mountdir):
	write_to_file('%s/file.txt' % mountdir, 'ahoj')

	os.mkdir('%s/tag1' % mountdir)
	assert 'tag1' in os.listdir(mountdir)
	assert 'tag2' not in os.listdir(mountdir)
	os.mkdir('%s/tag1/tag2' % mountdir)
	assert 'tag2' in os.listdir(mountdir)

	#os.rename('%s/tag1' % mountdir, 'tag111')
	#assert 'tag1' not in os.listdir(mountdir)
	#assert 'tag111' in os.listdir(mountdir)

	os.rename('%s/file.txt' % mountdir, '%s/tag2/file.txt' % mountdir)
	assert 'file.txt' in os.listdir(mountdir)
	assert 'file.txt' in os.listdir('%s/tag2' % mountdir)
	assert 'file.txt' not in os.listdir('%s/tag1' % mountdir)
