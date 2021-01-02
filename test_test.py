import pytest
import tempfile
import subprocess
import os
import time
import rptagfs


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
	with tempfile.TemporaryDirectory() as td:
		yield td


# TODO: find a way to deduplicate
@pytest.fixture
def tmpdir2():
	with tempfile.TemporaryDirectory() as td:
		yield td


@pytest.fixture
def mountdir(tmpdir, tmpdir2):
	cmd = './run_test.sh -d -oroot=%s %s >/tmp/logggg 2>/tmp/errlogggg' % (tmpdir2, tmpdir)
	#call(cmd)
	p = subprocess.Popen(cmd, shell=True)
	time.sleep(2)  # TODO: ugly hack
	p.poll()
	assert p.returncode is None
	yield tmpdir
	call('fusermount -u %s' % tmpdir)
	p.communicate()
	p.terminate()
	p.wait()


@pytest.fixture
def playdir__(tmpdir):
	wd = os.getcwd()
	os.chdir(tmpdir)
	os.mkdir('src')
	write_to_file('src/file1.txt', 'content1')
	write_to_file('src/file11.txt', 'content11')
	os.makedirs('src/tag1/tag2/tag3/empty_tag')
	write_to_file('src/tag1/tag2/tag3/file2.txt', 'content2')
	os.makedirs('src/tag_dup/tag_dup/tag_dup/tag1/tag_dup')
	write_to_file('src/tag_dup/tag_dup/tag_dup/tag1/tag_dup/file3.txt', 'content3')
	os.mkdir('mnt')
	os.chdir(wd)
	return tmpdir


@pytest.fixture
def playdir(tmpdir):
	os.mkdir('%s/src' % tmpdir)
	write_to_file('%s/src/file1.txt' % tmpdir, 'content1')
	write_to_file('%s/src/file11.txt' % tmpdir, 'content11')
	os.makedirs('%s/src/tag1/tag2/tag3/empty_tag' % tmpdir)
	write_to_file('%s/src/tag1/tag2/tag3/file2.txt' % tmpdir, 'content2')
	os.makedirs('%s/src/tag_dup/tag_dup/tag_dup/tag1/tag_dup' % tmpdir)
	write_to_file('%s/src/tag_dup/tag_dup/tag_dup/tag1/tag_dup/file3.txt' % tmpdir, 'content3')
	os.mkdir('%s/mnt' % tmpdir)
	return tmpdir


@pytest.fixture
def mntdir(playdir):
	os.chdir('/home/radek/work/rptagfs')  # TODO: hack
	cmd = './run_test.sh -d -oroot=%s/src %s/mnt >stdout 2>stderr' % (playdir, playdir)
	#call(cmd)
	p = subprocess.Popen(cmd, shell=True)
	time.sleep(2)  # TODO: ugly hack
	p.poll()
	assert p.returncode is None
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


def readdir_helper(x, path):
	ret = []
	for i in x.readdir(path, 0):
		ret.append(i.name)
	return ret


@pytest.fixture
def klasa(playdir):
	fff = rptagfs.RPTagFS()
	fff.root = '%s/src' % playdir
	os.chdir(fff.root)
	fff.init()
	fff.fsinit()
	return fff, playdir


def test_klasa_readdir(klasa):
	fff, dir_ = klasa
	assert 'file1.txt' in readdir_helper(fff, '/')
	assert 'file2.txt' in readdir_helper(fff, '/')
	assert 'file1.txt' not in readdir_helper(fff, '/tag1')
	assert 'file2.txt' in readdir_helper(fff, '/tag1')
	assert 'file2.txt' in readdir_helper(fff, '/tag2')
	assert 'file2.txt' in readdir_helper(fff, '/tag1/tag2')


def test_klasa_xxx_tag(klasa):
	fff, dir_ = klasa
	assert 'new_tag' not in readdir_helper(fff, '/')
	fff.mkdir('/new_tag', 0o777)
	assert 'new_tag' in readdir_helper(fff, '/')
	fff.rename('/new_tag', '/new_tag_xxx')
	assert 'new_tag' not in readdir_helper(fff, '/')
	assert 'new_tag_xxx' in readdir_helper(fff, '/')


def test_klasa_add_tag(klasa):
	fff, dir_ = klasa
	assert 'new_tag' not in readdir_helper(fff, '/')
	assert 'new_tag' not in os.listdir('%s/src' % dir_)
	fff.mkdir('/new_tag', 0o777)
	assert 'new_tag' in readdir_helper(fff, '/')
	assert 'new_tag' in os.listdir('%s/src' % dir_)


def test_klasa_add_tag_and_use_it(klasa):
	fff, dir_ = klasa
	assert 'new_tag' not in readdir_helper(fff, '/')
	assert 'new_tag' not in os.listdir('%s/src' % dir_)
	fff.mkdir('/new_tag', 0o777)
	assert 'new_tag' in readdir_helper(fff, '/')
	assert 'new_tag' in os.listdir('%s/src' % dir_)
	fff.rename('/file1.txt', '/new_tag/file1.txt')
	assert not os.path.isfile('%s/src/tag1/file1.txt' % dir_)
	assert os.path.isfile('%s/src/new_tag/file1.txt' % dir_)
	assert not os.path.isfile('%s/src/new_tag/file11.txt' % dir_)


def test_klasa_remove_tag(klasa):
	fff, dir_ = klasa
	assert 'empty_tag' in readdir_helper(fff, '/')
	assert os.path.isdir('%s/src/tag1/tag2/tag3/empty_tag' % dir_)
	fff.rmdir('/tag3/tag2/tag1/empty_tag')
	assert 'empty_tag' not in readdir_helper(fff, '/')
	assert not os.path.isdir('%s/src/tag1/tag2/tag3/empty_tag' % dir_)


def test_klasa_rename_tag(klasa):
	fff, dir_ = klasa
	assert 'tag1' in readdir_helper(fff, '/')
	assert 'tag11' not in readdir_helper(fff, '/')
	assert 'tag2' in readdir_helper(fff, '/')
	assert os.path.isdir('%s/src/tag1/tag2/tag3/empty_tag' % dir_)
	fff.rename('/tag1', '/tag11')
	assert 'tag1' not in readdir_helper(fff, '/')
	assert 'tag11' in readdir_helper(fff, '/')
	assert 'tag2' in readdir_helper(fff, '/')
	assert not os.path.isdir('%s/src/tag1/tag2/tag3/empty_tag' % dir_)
	assert os.path.isdir('%s/src/tag11/tag2/tag3/empty_tag' % dir_)


def test_klasa_rename_tag_dup(klasa):
	fff, dir_ = klasa
	assert 'tag_dup' in readdir_helper(fff, '/')
	assert 'tag_dup2' not in readdir_helper(fff, '/')
	assert os.path.isfile('%s/src/tag_dup/tag_dup/tag_dup/tag1/tag_dup/file3.txt' % dir_)
	fff.rename('/tag_dup', '/tag_dup2')
	assert 'tag_dup' not in readdir_helper(fff, '/')
	assert 'tag_dup2' in readdir_helper(fff, '/')
	assert os.path.isfile('%s/src/tag_dup2/tag_dup2/tag_dup2/tag1/tag_dup2/file3.txt' % dir_)
	assert not os.path.isfile('%s/src/tag_dup/tag_dup/tag_dup/tag1/tag_dup/file3.txt' % dir_)


def test_klasa_rename_file(klasa):
	fff, dir_ = klasa
	assert 'file1.txt' in readdir_helper(fff, '/')
	assert 'file1ren.txt' not in readdir_helper(fff, '/')
	assert os.path.isfile('%s/src/file1.txt' % dir_)
	fff.rename('/file1.txt', '/file1ren.txt')
	assert 'file1.txt' not in readdir_helper(fff, '/')
	assert 'file1ren.txt' in readdir_helper(fff, '/')
	assert not os.path.isfile('%s/src/file1.txt' % dir_)
	assert os.path.isfile('%s/src/file1ren.txt' % dir_)


def test_klasa_rename_file_with_tags(klasa):
	fff, dir_ = klasa
	assert 'file2.txt' in readdir_helper(fff, '/')
	assert 'file22.txt' not in readdir_helper(fff, '/')
	assert os.path.isfile('%s/src/tag1/tag2/tag3/file2.txt' % dir_)
	fff.rename('/file2.txt', '/file22.txt')
	assert 'file2.txt' not in readdir_helper(fff, '/')
	assert 'file22.txt' in readdir_helper(fff, '/')
	assert not os.path.isfile('%s/src/tag1/tag2/tag3/file2.txt' % dir_)
	assert os.path.isfile('%s/src/tag1/tag2/tag3/file22.txt' % dir_)


def test_playdir(mntdir):
	os.chdir(mntdir)
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
	print(os.listdir(mntdir))
	os.chdir(mntdir)
	print(os.listdir('src'), os.listdir('mnt'))
	assert 'tag_dup' in os.listdir('mnt')
	assert 'tag_dup2' not in os.listdir('mnt')
	os.rename('mnt/tag_dup', 'mnt/tag_dup2')
	assert 'tag_dup' not in os.listdir('mnt')
	assert 'tag_dup2' in os.listdir('mnt')
	assert read_from_file('mnt/tag_dup2/file3.txt') == 'content3'
	assert read_from_file('src/tag_dup2/tag_dup2/tag_dup2/tag1/tag_dup2/file3.txt') == 'content3'


def test_rename_tag(mntdir):
	assert 'tag1' in os.listdir('mnt')
	assert 'tag11' not in os.listdir('mnt')
	os.rename('mnt/tag1', 'mnt/tag11')
	#call('mv -v mnt/tag1 mnt/tag11')
	assert 'tag1' not in os.listdir('mnt')
	assert 'tag11' in os.listdir('mnt')


def test_rename_tag_deep(mntdir):
	assert 'tag3' in os.listdir('mnt/tag1/tag2')
	assert 'tag33' not in os.listdir('mnt/tag1/tag2')
	os.rename('mnt/tag1/tag2/tag3', 'mnt/tag1/tag2/tag33')
	#call('mv -v mnt/tag1 mnt/tag11')
	assert 'tag3' not in os.listdir('mnt/tag1/tag2')
	assert 'tag33' in os.listdir('mnt/tag1/tag2')


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


def test_create_file(mntdir):
	#assert get_dir_contents(mountdir) == []
	#print(call('echo "ahoj" >%s/pokus.txt' % mountdir))
	#print(call('ls -l %s' % mountdir))

	assert 'file.txt' not in os.listdir('%s/mnt' % mntdir)
	write_to_file('%s/mnt/file.txt' % mntdir, 'ahoj')
	assert read_from_file('%s/mnt/file.txt' % mntdir) == 'ahoj'
	assert 'file.txt' in os.listdir('%s/mnt' % mntdir)


def test_add_tags(mntdir):
	write_to_file('%s/mnt/file.txt' % mntdir, 'ahoj')

	os.mkdir('%s/mnt/tagAAA' % mntdir)
	assert 'tagAAA' in os.listdir('%s/mnt' % mntdir)
	assert 'tagBBB' not in os.listdir('%s/mnt' % mntdir)
	os.mkdir('%s/mnt/tag1/tagBBB' % mntdir)
	assert 'tagBBB' in os.listdir('%s/mnt' % mntdir)

	#os.rename('%s/tag1' % mountdir, 'tag111')
	#assert 'tag1' not in os.listdir(mountdir)
	#assert 'tag111' in os.listdir(mountdir)

	os.rename('%s/mnt/file.txt' % mntdir, '%s/mnt/tagBBB/file.txt' % mntdir)
	assert 'file.txt' in os.listdir('%s/mnt' % mntdir)
	assert 'file.txt' in os.listdir('%s/mnt/tagBBB' % mntdir)
	assert 'file.txt' not in os.listdir('%s/mnt/tagAAA' % mntdir)
